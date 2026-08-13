//! Editing a windowed calibration that already sits in the past.
//!
//! Scenario: an operator corrects a calibration after the data it covers has landed, by changing
//! its coefficients, by moving `valid_from` in either direction, or by inserting a new curve into
//! the middle of an existing window.
//!
//! Expected behaviour: every reading the changed window covers is rewritten to the new curve,
//! every reading outside it keeps both its previous `calibrated_value` and its previous
//! `calibration_id`, `valid_until` re-chains across the neighbouring windows, the half-open
//! `[valid_from, valid_until)` interval is honoured to the microsecond, and the continuous
//! aggregates covering the changed window report the recalculated means while a bucket outside it
//! recomputes to the same number.
//!
//! `reprocess_sensor_readings` rewrites ALL of a sensor's non-spot readings in one statement, so an
//! "unaffected" assertion here says that recomputation lands on the same value and the same
//! `calibration_id`, not that the row was skipped. That is the window-selection logic under test,
//! which is why every expectation below covers every row rather than only the moved ones.
//!
//! Every entity is provisioned over HTTP from the sensor-flow onboarding track, as the roles that
//! own each step: administrator for inventory, manager for calibrations and deployments, river for
//! ingestion. Direct SQL appears only to read `calibration_id`/`deployment_id`, which no endpoint
//! exposes.

use chrono::{DateTime, Utc};
use serde_json::json;
use serial_test::serial;
use std::time::Duration;
use uuid::Uuid;

use crate::common::keycloak as kc;
use crate::common::sensor_lifecycle::{ReadingRow, get_readings, wait_for_reprocessing};
use crate::common::tracks;
use crate::common::{
    e2e, get_json_with_token, post_json_parse_with_token, post_json_with_token, put_json_with_token,
};

const WAIT: Duration = Duration::from_secs(30);
/// Every asserted value is exactly representable in binary; the tolerance guards only against a
/// future fixture whose arithmetic is not, since `avg_value` is a floating point AVG.
const EPS: f64 = 1e-9;

fn ts(s: &str) -> DateTime<Utc> {
    s.parse()
        .unwrap_or_else(|e| panic!("invalid fixture timestamp '{s}': {e}"))
}

struct Fixture {
    db: sea_orm::DatabaseConnection,
    app: axum::Router,
    admin: String,
    manager: String,
    river: String,
    track: tracks::Track,
    sensor_id: String,
    stream_id: String,
}

impl Fixture {
    fn sensor(&self) -> Uuid {
        Uuid::parse_str(&self.sensor_id).expect("sensor id is a uuid")
    }

    fn stream(&self) -> Uuid {
        Uuid::parse_str(&self.stream_id).expect("stream id is a uuid")
    }

    fn parameter(&self) -> &str {
        &self.track.parameters[0].1
    }
}

/// Track B, plus the stream/sensor link and the pairing the calibration timeline needs.
///
/// `POST /streams/register` carries no sensor field, so the registered stream is attached to the
/// track's sensor through the `data_streams` CRUD surface before pairing. Pairing then reuses that
/// sensor and its open deployment instead of minting a second sensor for the same feed.
async fn onboard() -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;

    kc::ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;

    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    let track = tracks::onboard_sensor_flow_track(&app, &admin).await;

    kc::grant_project(
        &db,
        &kc::keycloak_user_id("manager1").await,
        &track.project_id,
    )
    .await;
    kc::grant_project(
        &db,
        &kc::keycloak_user_id("river1").await,
        &track.project_id,
    )
    .await;
    let manager = kc::get_keycloak_jwt("manager1", "manager1").await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;

    let sensor_id = track
        .sensor_id
        .clone()
        .expect("the sensor-flow track provisions a sensor");
    let stream_id = track.stream_ids[0].clone();

    e2e::link_stream_sensor(&app, &admin, &stream_id, &sensor_id).await;

    let (status, body) = post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({ "site_parameter_id": track.site_parameter_ids[0] }),
        &admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "the stream pairs to the site slot ({status}): {body}"
    );

    Fixture {
        db,
        app,
        admin,
        manager,
        river,
        track,
        sensor_id,
        stream_id,
    }
}

/// Create a windowed curve for the track's sensor and parameter, as the manager who owns sensor
/// movement. `parameter_id` is always sent: the curve must join its parameter's chaining partition.
async fn add_curve(fx: &Fixture, slope: f64, intercept: f64, valid_from: &str) -> Uuid {
    let (status, body) = post_json_parse_with_token(
        &fx.app,
        "/api/sensor_calibrations",
        &json!({
            "sensor_id": fx.sensor_id,
            "parameter_id": fx.parameter(),
            "slope": slope,
            "intercept": intercept,
            "valid_from": valid_from,
        }),
        &fx.manager,
    )
    .await;
    assert_eq!(
        status, 201,
        "manager creates a curve from {valid_from}: {body}"
    );
    assert!(
        wait_for_reprocessing(&fx.db, fx.sensor(), WAIT).await,
        "the calibration_create job settles without failing"
    );
    Uuid::parse_str(&e2e::id_of(&body)).expect("calibration id is a uuid")
}

/// Ingest raw readings as the river-level user who owns data entry.
///
/// The ingest path applies whatever curve covers the reading's time at write time, and writes a
/// null `calibrated_value` when none does. `reprocess` below is what turns this into the pre-edit
/// state each test asserts.
async fn ingest(fx: &Fixture, stream_id: &str, rows: &[(&str, f64)]) {
    let readings: Vec<serde_json::Value> = rows
        .iter()
        .map(|(time, raw)| json!({ "time": time, "raw_value": raw }))
        .collect();
    let (status, body) = post_json_parse_with_token(
        &fx.app,
        "/api/ingest",
        &json!({ "stream_id": stream_id, "readings": readings }),
        &fx.river,
    )
    .await;
    assert_eq!(status, 200, "river ingests {} readings: {body}", rows.len());
    assert_eq!(
        body["inserted"].as_u64(),
        Some(rows.len() as u64),
        "every fixture reading lands: {body}"
    );
    assert_eq!(
        body["paired"], true,
        "the stream is paired, so readings land attributed: {body}"
    );
}

async fn reprocess(fx: &Fixture) {
    let (status, body) = post_json_with_token(
        &fx.app,
        "/api/actions/reprocess",
        &json!({ "sensor_id": fx.sensor_id }),
        &fx.manager,
    )
    .await;
    assert_eq!(status, 200, "manager triggers a reprocess: {body}");
    assert!(
        wait_for_reprocessing(&fx.db, fx.sensor(), WAIT).await,
        "the reprocess job settles without failing"
    );
}

/// `(valid_from, valid_until)` as the API serves them.
async fn window_of(fx: &Fixture, cal: Uuid) -> (DateTime<Utc>, Option<DateTime<Utc>>) {
    let (status, body) = get_json_with_token(
        &fx.app,
        &format!("/api/sensor_calibrations/{cal}"),
        &fx.manager,
    )
    .await;
    assert_eq!(status, 200, "read calibration {cal}: {body}");
    let from = body["valid_from"]
        .as_str()
        .unwrap_or_else(|| panic!("calibration {cal} must serve valid_from: {body}"));
    // Read the key explicitly: a missing field would otherwise be indistinguishable from a null
    // one, and "the newest window is open-ended" would then pass against a response that never
    // carried the boundary at all.
    assert!(
        body.get("valid_until").is_some(),
        "calibration {cal} must serve valid_until, null or not: {body}"
    );
    let until = body["valid_until"].as_str().map(ts);
    (ts(from), until)
}

async fn completed_jobs(fx: &Fixture, trigger: &str) -> usize {
    let filter = e2e::percent_encode(&format!(
        r#"{{"trigger_type":"{trigger}","sensor_id":"{}","status":"completed"}}"#,
        fx.sensor_id
    ));
    let (status, body) = get_json_with_token(
        &fx.app,
        &format!("/api/reprocessing_jobs?filter={filter}"),
        &fx.manager,
    )
    .await;
    assert_eq!(status, 200, "list completed {trigger} jobs: {body}");
    body.as_array()
        .unwrap_or_else(|| panic!("the jobs list must be an array: {body}"))
        .len()
}

/// Expected `(time, raw_value, calibrated_value, calibration_id)` for every reading on a stream.
/// Covering every row is what makes the untouched windows a real control rather than an omission.
fn check(rows: &[ReadingRow], expected: &[(&str, f64, f64, Uuid)], phase: &str) {
    assert_eq!(
        rows.len(),
        expected.len(),
        "{phase}: expected {} readings, found {}",
        expected.len(),
        rows.len()
    );
    for (i, (time, raw, calibrated, cal_id)) in expected.iter().enumerate() {
        assert_eq!(rows[i].time, ts(time), "{phase} row {i}: time");
        assert_eq!(
            rows[i].raw_value, *raw,
            "{phase} row {i} at {time}: raw_value must never be rewritten"
        );
        assert_eq!(
            rows[i].calibrated_value,
            Some(*calibrated),
            "{phase} row {i} at {time}: calibrated_value"
        );
        assert_eq!(
            rows[i].calibration_id,
            Some(*cal_id),
            "{phase} row {i} at {time}: calibration_id"
        );
    }
}

async fn hourly(fx: &Fixture, site: &str, at: &str, why: &str) -> (f64, i64) {
    let bucket = e2e::hourly_bucket(&fx.db, site, fx.parameter(), ts(at)).await;
    assert!(
        bucket.is_some(),
        "{why}: no hourly bucket materialised at {at} for site {site}"
    );
    bucket.expect("bucket presence asserted above")
}

fn assert_bucket(actual: (f64, i64), mean: f64, count: i64, why: &str) {
    assert!(
        (actual.0 - mean).abs() < EPS,
        "{why}: expected hourly mean {mean}, got {}",
        actual.0
    );
    assert_eq!(actual.1, count, "{why}: hourly count");
}

/// Coefficients change on a curve bounded on both sides. Only its own window is rewritten, the
/// neighbouring windows keep their values AND their `calibration_id`, no boundary moves, and the
/// hourly aggregate over the changed window reports the recalculated mean.
#[tokio::test]
#[serial]
async fn editing_a_historical_calibrations_coefficients_rewrites_only_its_own_window() {
    if !kc::require_keycloak_or_skip("calibration_coefficient_edit").await {
        return;
    }
    let fx = onboard().await;

    let c0 = add_curve(&fx, 2.0, 0.0, "2025-06-05T00:00:00Z").await;
    let c1 = add_curve(&fx, 10.0, 1.0, "2025-06-10T00:00:00Z").await;
    let c2 = add_curve(&fx, 3.0, 0.0, "2025-06-20T00:00:00Z").await;

    ingest(
        &fx,
        &fx.stream_id,
        &[
            ("2025-06-06T12:00:00Z", 204.0),
            ("2025-06-12T10:00:00Z", 205.0),
            ("2025-06-12T10:30:00Z", 207.0),
            ("2025-06-25T12:00:00Z", 206.0),
        ],
    )
    .await;
    reprocess(&fx).await;
    e2e::refresh_hourly(&fx.db, ts("2025-06-01T00:00:00Z")).await;

    let site = fx.track.site_id.clone();
    check(
        &get_readings(&fx.db, fx.stream()).await,
        &[
            ("2025-06-06T12:00:00Z", 204.0, 408.0, c0),  // 2 * 204
            ("2025-06-12T10:00:00Z", 205.0, 2051.0, c1), // 10 * 205 + 1
            ("2025-06-12T10:30:00Z", 207.0, 2071.0, c1), // 10 * 207 + 1
            ("2025-06-25T12:00:00Z", 206.0, 618.0, c2),  // 3 * 206
        ],
        "before the edit",
    );
    assert_eq!(
        window_of(&fx, c0).await,
        (ts("2025-06-05T00:00:00Z"), Some(ts("2025-06-10T00:00:00Z"))),
        "before the edit: C0 is bounded by C1's start"
    );
    assert_eq!(
        window_of(&fx, c1).await,
        (ts("2025-06-10T00:00:00Z"), Some(ts("2025-06-20T00:00:00Z"))),
        "before the edit: C1 is bounded on both sides"
    );
    assert_eq!(
        window_of(&fx, c2).await,
        (ts("2025-06-20T00:00:00Z"), None),
        "before the edit: the newest curve stays open-ended"
    );

    assert_bucket(
        hourly(&fx, &site, "2025-06-12T10:00:00Z", "before the edit").await,
        2061.0, // (2051 + 2071) / 2
        2,
        "before the edit: the bucket inside C1's window",
    );
    assert_bucket(
        hourly(&fx, &site, "2025-06-06T12:00:00Z", "before the edit").await,
        408.0,
        1,
        "before the edit: the bucket inside C0's window",
    );
    assert_bucket(
        hourly(&fx, &site, "2025-06-25T12:00:00Z", "before the edit").await,
        618.0,
        1,
        "before the edit: the bucket inside C2's window",
    );

    let (status, body) = put_json_with_token(
        &fx.app,
        &format!("/api/sensor_calibrations/{c1}"),
        &json!({ "slope": 4.0, "intercept": 0.5 }),
        &fx.river,
    )
    .await;
    assert_eq!(
        status, 403,
        "editing a calibration is manager work; the river level below it is refused: {body}"
    );

    let (status, body) = put_json_with_token(
        &fx.app,
        &format!("/api/sensor_calibrations/{c1}"),
        &json!({ "slope": 4.0, "intercept": 0.5 }),
        &fx.manager,
    )
    .await;
    assert_eq!(status, 200, "manager corrects C1's coefficients: {body}");
    assert!(
        wait_for_reprocessing(&fx.db, fx.sensor(), WAIT).await,
        "the calibration_update job settles without failing"
    );
    assert_eq!(
        completed_jobs(&fx, "calibration_update").await,
        1,
        "one edit enqueues exactly one completed calibration_update job for this sensor"
    );

    check(
        &get_readings(&fx.db, fx.stream()).await,
        &[
            // Untouched: an indiscriminate rewrite under C1's new curve would give 816.5 here.
            ("2025-06-06T12:00:00Z", 204.0, 408.0, c0),
            ("2025-06-12T10:00:00Z", 205.0, 820.5, c1), // 4 * 205 + 0.5
            ("2025-06-12T10:30:00Z", 207.0, 828.5, c1), // 4 * 207 + 0.5
            // Untouched: the same rewrite would give 824.5 here.
            ("2025-06-25T12:00:00Z", 206.0, 618.0, c2),
        ],
        "after the edit",
    );

    assert_eq!(
        window_of(&fx, c0).await,
        (ts("2025-06-05T00:00:00Z"), Some(ts("2025-06-10T00:00:00Z"))),
        "a coefficient edit moves no boundary: C0 keeps its window"
    );
    assert_eq!(
        window_of(&fx, c1).await,
        (ts("2025-06-10T00:00:00Z"), Some(ts("2025-06-20T00:00:00Z"))),
        "a coefficient edit moves no boundary: C1 keeps its window"
    );
    assert_eq!(
        window_of(&fx, c2).await,
        (ts("2025-06-20T00:00:00Z"), None),
        "a coefficient edit moves no boundary: C2 stays open-ended"
    );

    // The job's own refresh runs from the sensor's earliest reading to now, so all three buckets
    // are re-materialised. The two unchanged ones therefore prove the readings under C0 and C2
    // recompute to the same numbers, not merely that their buckets were left alone.
    assert_bucket(
        hourly(&fx, &site, "2025-06-12T10:00:00Z", "after the edit").await,
        824.5, // (820.5 + 828.5) / 2
        2,
        "the aggregate over the edited window reports the recalculated mean",
    );
    assert_bucket(
        hourly(&fx, &site, "2025-06-06T12:00:00Z", "after the edit").await,
        408.0,
        1,
        "the aggregate inside C0's window is unchanged",
    );
    assert_bucket(
        hourly(&fx, &site, "2025-06-25T12:00:00Z", "after the edit").await,
        618.0,
        1,
        "the aggregate inside C2's window is unchanged",
    );

    let (status, served) = get_json_with_token(
        &fx.app,
        &format!("/api/sites/{site}/readings?start=2025-06-01T00:00:00Z&end=2025-07-01T00:00:00Z"),
        &fx.river,
    )
    .await;
    assert_eq!(status, 200, "served readings ({status}): {served}");
    assert_eq!(
        e2e::values_for(&served, fx.parameter()),
        vec![408.0, 820.5, 828.5, 618.0],
        "the served series carries the corrected curve: {served}"
    );
}

/// Widening a curve backwards reclaims the readings the previous window held: they take the new
/// curve's value and its `calibration_id`, the previous window shrinks to the new boundary, and the
/// curve's own right-hand boundary does not move.
#[tokio::test]
#[serial]
async fn moving_valid_from_earlier_reclaims_readings_from_the_previous_window() {
    if !kc::require_keycloak_or_skip("calibration_valid_from_earlier").await {
        return;
    }
    let fx = onboard().await;

    let c0 = add_curve(&fx, 2.0, 0.0, "2025-06-05T00:00:00Z").await;
    let c1 = add_curve(&fx, 10.0, 0.0, "2025-06-20T00:00:00Z").await;
    let c2 = add_curve(&fx, 100.0, 0.0, "2025-06-25T00:00:00Z").await;

    ingest(
        &fx,
        &fx.stream_id,
        &[
            ("2025-06-08T12:00:00Z", 202.0),
            ("2025-06-14T12:00:00Z", 203.0),
            ("2025-06-18T12:00:00Z", 204.0),
            ("2025-06-22T12:00:00Z", 205.0),
            ("2025-06-27T12:00:00Z", 206.0),
        ],
    )
    .await;
    reprocess(&fx).await;

    check(
        &get_readings(&fx.db, fx.stream()).await,
        &[
            ("2025-06-08T12:00:00Z", 202.0, 404.0, c0),   // 2 * 202
            ("2025-06-14T12:00:00Z", 203.0, 406.0, c0),   // 2 * 203
            ("2025-06-18T12:00:00Z", 204.0, 408.0, c0),   // 2 * 204
            ("2025-06-22T12:00:00Z", 205.0, 2050.0, c1),  // 10 * 205
            ("2025-06-27T12:00:00Z", 206.0, 20600.0, c2), // 100 * 206
        ],
        "before the move",
    );
    assert_eq!(
        window_of(&fx, c0).await,
        (ts("2025-06-05T00:00:00Z"), Some(ts("2025-06-20T00:00:00Z"))),
        "before the move: C0 runs up to C1's start"
    );
    assert_eq!(
        window_of(&fx, c1).await,
        (ts("2025-06-20T00:00:00Z"), Some(ts("2025-06-25T00:00:00Z"))),
        "before the move: C1 is bounded by C2"
    );

    let (status, body) = put_json_with_token(
        &fx.app,
        &format!("/api/sensor_calibrations/{c1}"),
        &json!({ "valid_from": "2025-06-12T00:00:00Z" }),
        &fx.manager,
    )
    .await;
    assert_eq!(status, 200, "manager backdates C1's start: {body}");
    assert!(
        wait_for_reprocessing(&fx.db, fx.sensor(), WAIT).await,
        "the calibration_update job settles without failing"
    );

    check(
        &get_readings(&fx.db, fx.stream()).await,
        &[
            // Before the new boundary, so it stays on C0 in value and in FK.
            ("2025-06-08T12:00:00Z", 202.0, 404.0, c0),
            ("2025-06-14T12:00:00Z", 203.0, 2030.0, c1), // reclaimed: 10 * 203
            ("2025-06-18T12:00:00Z", 204.0, 2040.0, c1), // reclaimed: 10 * 204
            ("2025-06-22T12:00:00Z", 205.0, 2050.0, c1), // already C1, unchanged
            ("2025-06-27T12:00:00Z", 206.0, 20600.0, c2), // right of C1, unchanged
        ],
        "after the move",
    );

    assert_eq!(
        window_of(&fx, c0).await,
        (ts("2025-06-05T00:00:00Z"), Some(ts("2025-06-12T00:00:00Z"))),
        "the previous window shrinks to the new boundary"
    );
    assert_eq!(
        window_of(&fx, c1).await,
        (ts("2025-06-12T00:00:00Z"), Some(ts("2025-06-25T00:00:00Z"))),
        "widening C1 backwards must not move its right-hand boundary"
    );
    assert_eq!(
        window_of(&fx, c2).await,
        (ts("2025-06-25T00:00:00Z"), None),
        "the newest curve stays open-ended"
    );
}

/// Shrinking a curve from the left hands the uncovered readings back to the previous window rather
/// than leaving them window-less: calibration windows absorb the gap, so the previous window's
/// `valid_until` extends to the new boundary.
#[tokio::test]
#[serial]
async fn moving_valid_from_later_returns_the_uncovered_readings_to_the_previous_window() {
    if !kc::require_keycloak_or_skip("calibration_valid_from_later").await {
        return;
    }
    let fx = onboard().await;

    let c0 = add_curve(&fx, 2.0, 0.0, "2025-06-05T00:00:00Z").await;
    let c1 = add_curve(&fx, 10.0, 0.0, "2025-06-20T00:00:00Z").await;
    let c2 = add_curve(&fx, 100.0, 0.0, "2025-06-25T00:00:00Z").await;

    ingest(
        &fx,
        &fx.stream_id,
        &[
            ("2025-06-08T12:00:00Z", 202.0),
            ("2025-06-21T12:00:00Z", 203.0),
            ("2025-06-22T12:00:00Z", 204.0),
            ("2025-06-24T12:00:00Z", 205.0),
            ("2025-06-27T12:00:00Z", 206.0),
        ],
    )
    .await;
    reprocess(&fx).await;

    check(
        &get_readings(&fx.db, fx.stream()).await,
        &[
            ("2025-06-08T12:00:00Z", 202.0, 404.0, c0),
            ("2025-06-21T12:00:00Z", 203.0, 2030.0, c1),
            ("2025-06-22T12:00:00Z", 204.0, 2040.0, c1),
            ("2025-06-24T12:00:00Z", 205.0, 2050.0, c1),
            ("2025-06-27T12:00:00Z", 206.0, 20600.0, c2),
        ],
        "before the move",
    );
    assert_eq!(
        window_of(&fx, c0).await,
        (ts("2025-06-05T00:00:00Z"), Some(ts("2025-06-20T00:00:00Z"))),
        "before the move: C0 runs up to C1's start"
    );

    let (status, body) = put_json_with_token(
        &fx.app,
        &format!("/api/sensor_calibrations/{c1}"),
        &json!({ "valid_from": "2025-06-24T00:00:00Z" }),
        &fx.manager,
    )
    .await;
    assert_eq!(status, 200, "manager pushes C1's start forward: {body}");
    assert!(
        wait_for_reprocessing(&fx.db, fx.sensor(), WAIT).await,
        "the calibration_update job settles without failing"
    );

    check(
        &get_readings(&fx.db, fx.stream()).await,
        &[
            ("2025-06-08T12:00:00Z", 202.0, 404.0, c0), // always inside C0, unchanged
            ("2025-06-21T12:00:00Z", 203.0, 406.0, c0), // handed back: 2 * 203
            ("2025-06-22T12:00:00Z", 204.0, 408.0, c0), // handed back: 2 * 204
            ("2025-06-24T12:00:00Z", 205.0, 2050.0, c1), // at/after the new start, still C1
            ("2025-06-27T12:00:00Z", 206.0, 20600.0, c2),
        ],
        "after the move",
    );

    assert_eq!(
        window_of(&fx, c0).await,
        (ts("2025-06-05T00:00:00Z"), Some(ts("2025-06-24T00:00:00Z"))),
        "the vacated interval is absorbed by the previous window, so no reading is left uncovered"
    );
    assert_eq!(
        window_of(&fx, c1).await,
        (ts("2025-06-24T00:00:00Z"), Some(ts("2025-06-25T00:00:00Z"))),
        "C1's right-hand boundary is unchanged"
    );
    assert_eq!(
        window_of(&fx, c2).await,
        (ts("2025-06-25T00:00:00Z"), None),
        "the newest curve stays open-ended"
    );
}

/// A curve inserted into the middle of an existing window splits the chain and takes exactly the
/// readings its half-open `[valid_from, valid_until)` interval covers, pinned to the microsecond:
/// the instant AT `valid_from` belongs to the new window, the microsecond before it does not, and
/// the instant AT `valid_until` belongs to the next window.
#[tokio::test]
#[serial]
async fn inserting_a_calibration_mid_window_splits_it_on_half_open_boundaries() {
    if !kc::require_keycloak_or_skip("calibration_mid_window_insert").await {
        return;
    }
    let fx = onboard().await;

    let c1 = add_curve(&fx, 2.0, 0.0, "2025-06-03T00:00:00Z").await;
    let c2 = add_curve(&fx, 3.0, 0.0, "2025-06-20T00:00:00Z").await;

    ingest(
        &fx,
        &fx.stream_id,
        &[
            ("2025-06-05T00:00:00Z", 201.0),
            ("2025-06-09T23:59:59.999999Z", 202.0),
            ("2025-06-10T00:00:00Z", 203.0),
            ("2025-06-15T00:00:00Z", 204.0),
            ("2025-06-19T23:59:59.999999Z", 205.0),
            ("2025-06-20T00:00:00Z", 206.0),
            ("2025-06-25T00:00:00Z", 207.0),
        ],
    )
    .await;
    reprocess(&fx).await;

    check(
        &get_readings(&fx.db, fx.stream()).await,
        &[
            ("2025-06-05T00:00:00Z", 201.0, 402.0, c1),
            ("2025-06-09T23:59:59.999999Z", 202.0, 404.0, c1),
            ("2025-06-10T00:00:00Z", 203.0, 406.0, c1),
            ("2025-06-15T00:00:00Z", 204.0, 408.0, c1),
            ("2025-06-19T23:59:59.999999Z", 205.0, 410.0, c1),
            ("2025-06-20T00:00:00Z", 206.0, 618.0, c2), // 3 * 206
            ("2025-06-25T00:00:00Z", 207.0, 621.0, c2), // 3 * 207
        ],
        "before the insert",
    );
    assert_eq!(
        window_of(&fx, c1).await,
        (ts("2025-06-03T00:00:00Z"), Some(ts("2025-06-20T00:00:00Z"))),
        "before the insert: C1 owns the whole span C_mid will be dropped into"
    );

    let (status, body) = post_json_parse_with_token(
        &fx.app,
        "/api/sensor_calibrations",
        &json!({
            "sensor_id": fx.sensor_id,
            "parameter_id": fx.parameter(),
            "slope": 10.0,
            "intercept": 0.0,
            "valid_from": "2025-06-10T00:00:00Z",
        }),
        &fx.river,
    )
    .await;
    assert_eq!(
        status, 403,
        "creating a calibration is manager work; the river level below it is refused: {body}"
    );

    let c_mid = add_curve(&fx, 10.0, 0.0, "2025-06-10T00:00:00Z").await;

    check(
        &get_readings(&fx.db, fx.stream()).await,
        &[
            // Left of the split, untouched in value and in FK.
            ("2025-06-05T00:00:00Z", 201.0, 402.0, c1),
            // One microsecond before valid_from: the new window must not reach it.
            ("2025-06-09T23:59:59.999999Z", 202.0, 404.0, c1),
            // Exactly at valid_from: inclusive lower bound, so 10 * 203 rather than C1's 406.0.
            ("2025-06-10T00:00:00Z", 203.0, 2030.0, c_mid),
            ("2025-06-15T00:00:00Z", 204.0, 2040.0, c_mid),
            // The last representable instant strictly inside the new window.
            ("2025-06-19T23:59:59.999999Z", 205.0, 2050.0, c_mid),
            // Exactly at valid_until: exclusive upper bound, so C2's 618.0 rather than 2060.0.
            ("2025-06-20T00:00:00Z", 206.0, 618.0, c2),
            ("2025-06-25T00:00:00Z", 207.0, 621.0, c2),
        ],
        "after the insert",
    );

    assert_eq!(
        window_of(&fx, c1).await,
        (ts("2025-06-03T00:00:00Z"), Some(ts("2025-06-10T00:00:00Z"))),
        "the host window is cut back to the inserted curve's start"
    );
    assert_eq!(
        window_of(&fx, c_mid).await,
        (ts("2025-06-10T00:00:00Z"), Some(ts("2025-06-20T00:00:00Z"))),
        "the inserted curve joins the parameter's chain and is bounded by C2"
    );
    assert_eq!(
        window_of(&fx, c2).await,
        (ts("2025-06-20T00:00:00Z"), None),
        "C2 keeps its start and stays open-ended"
    );
}

/// One calibration window spanning two deployments at two sites. A single coefficient edit must
/// rewrite the readings at both sites, and both sites' hourly aggregates must report the
/// recalculated means, while a bucket in the preceding curve's window recomputes unchanged.
#[tokio::test]
#[serial]
async fn editing_a_calibration_spanning_two_deployments_updates_both_sites_aggregates() {
    if !kc::require_keycloak_or_skip("calibration_spanning_two_sites").await {
        return;
    }
    let fx = onboard().await;
    let site1 = fx.track.site_id.clone();
    let dep1 = Uuid::parse_str(
        fx.track
            .deployment_id
            .as_ref()
            .expect("the track opens a deployment"),
    )
    .expect("deployment id is a uuid");

    let site2 = e2e::create_site(
        &fx.app,
        &fx.admin,
        &fx.track.project_id,
        "Site flow downstream",
        "site-flow-downstream",
    )
    .await;
    let sp2 =
        e2e::assign_site_parameter_minimal(&fx.app, &fx.manager, &site2, fx.parameter()).await;

    let (status, body) = post_json_parse_with_token(
        &fx.app,
        "/api/sensor_deployments",
        &json!({
            "sensor_id": fx.sensor_id,
            "site_id": site2,
            "parameter_id": fx.parameter(),
            "deployed_from": "2025-06-10T02:00:00Z",
        }),
        &fx.manager,
    )
    .await;
    assert_eq!(
        status, 201,
        "manager moves the sensor to the second site: {body}"
    );
    let dep2 = Uuid::parse_str(&e2e::id_of(&body)).expect("deployment id is a uuid");
    assert!(
        wait_for_reprocessing(&fx.db, fx.sensor(), WAIT).await,
        "the deployment_create job settles without failing"
    );

    let (status, body) = post_json_parse_with_token(
        &fx.app,
        "/api/streams/register",
        &json!({
            "source_system": "trk_flow",
            "source_key": "trk-flow-do-2",
            "source_name": "Track flow DO downstream",
        }),
        &fx.admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "register the second stream ({status}): {body}"
    );
    let stream2 = e2e::id_of(&body);
    e2e::link_stream_sensor(&fx.app, &fx.admin, &stream2, &fx.sensor_id).await;
    let (status, body) = post_json_with_token(
        &fx.app,
        &format!("/api/streams/{stream2}/pair"),
        &json!({ "site_parameter_id": sp2 }),
        &fx.admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "the second stream pairs to the second site's slot ({status}): {body}"
    );

    // C1 starts two hours before the site change, so its window straddles both deployments.
    let c0 = add_curve(&fx, 5.0, 0.0, "2025-06-02T00:00:00Z").await;
    let c1 = add_curve(&fx, 2.0, 0.0, "2025-06-10T00:00:00Z").await;

    ingest(
        &fx,
        &fx.stream_id,
        &[
            ("2025-06-05T09:00:00Z", 204.0),
            ("2025-06-05T09:30:00Z", 206.0),
            ("2025-06-10T00:15:00Z", 210.0),
            ("2025-06-10T00:45:00Z", 220.0),
        ],
    )
    .await;
    ingest(
        &fx,
        &stream2,
        &[
            ("2025-06-10T02:15:00Z", 230.0),
            ("2025-06-10T02:45:00Z", 250.0),
        ],
    )
    .await;
    reprocess(&fx).await;
    e2e::refresh_hourly(&fx.db, ts("2025-06-01T00:00:00Z")).await;

    let site1_uuid: Uuid = site1.parse().expect("site id is a uuid");
    let site2_uuid: Uuid = site2.parse().expect("site id is a uuid");
    let stream2_uuid: Uuid = stream2.parse().expect("stream id is a uuid");

    let upstream = get_readings(&fx.db, fx.stream()).await;
    check(
        &upstream,
        &[
            ("2025-06-05T09:00:00Z", 204.0, 1020.0, c0), // 5 * 204
            ("2025-06-05T09:30:00Z", 206.0, 1030.0, c0), // 5 * 206
            ("2025-06-10T00:15:00Z", 210.0, 420.0, c1),  // 2 * 210
            ("2025-06-10T00:45:00Z", 220.0, 440.0, c1),  // 2 * 220
        ],
        "before the edit, upstream site",
    );
    for (i, row) in upstream.iter().enumerate() {
        assert_eq!(
            row.site_id,
            Some(site1_uuid),
            "before the edit: upstream row {i} site"
        );
        assert_eq!(
            row.deployment_id,
            Some(dep1),
            "before the edit: upstream row {i} deployment"
        );
    }

    let downstream = get_readings(&fx.db, stream2_uuid).await;
    check(
        &downstream,
        &[
            ("2025-06-10T02:15:00Z", 230.0, 460.0, c1), // 2 * 230
            ("2025-06-10T02:45:00Z", 250.0, 500.0, c1), // 2 * 250
        ],
        "before the edit, downstream site",
    );
    for (i, row) in downstream.iter().enumerate() {
        assert_eq!(
            row.site_id,
            Some(site2_uuid),
            "before the edit: downstream row {i} site"
        );
        assert_eq!(
            row.deployment_id,
            Some(dep2),
            "before the edit: downstream row {i} deployment"
        );
    }

    assert_bucket(
        hourly(&fx, &site1, "2025-06-05T09:00:00Z", "before the edit").await,
        1025.0,
        2,
        "before the edit: the upstream bucket inside C0's window",
    );
    assert_bucket(
        hourly(&fx, &site1, "2025-06-10T00:00:00Z", "before the edit").await,
        430.0,
        2,
        "before the edit: the upstream bucket inside C1's window",
    );
    assert_bucket(
        hourly(&fx, &site2, "2025-06-10T02:00:00Z", "before the edit").await,
        480.0,
        2,
        "before the edit: the downstream bucket inside C1's window",
    );

    let (status, body) = put_json_with_token(
        &fx.app,
        &format!("/api/sensor_calibrations/{c1}"),
        &json!({ "slope": 4.0, "intercept": 1.0 }),
        &fx.manager,
    )
    .await;
    assert_eq!(status, 200, "manager corrects the spanning curve: {body}");
    assert!(
        wait_for_reprocessing(&fx.db, fx.sensor(), WAIT).await,
        "the calibration_update job settles without failing"
    );
    assert_eq!(
        completed_jobs(&fx, "calibration_update").await,
        1,
        "one edit enqueues exactly one completed calibration_update job for this sensor"
    );

    let upstream = get_readings(&fx.db, fx.stream()).await;
    check(
        &upstream,
        &[
            // Under C0, so unchanged in value and in FK.
            ("2025-06-05T09:00:00Z", 204.0, 1020.0, c0),
            ("2025-06-05T09:30:00Z", 206.0, 1030.0, c0),
            ("2025-06-10T00:15:00Z", 210.0, 841.0, c1), // 4 * 210 + 1
            ("2025-06-10T00:45:00Z", 220.0, 881.0, c1), // 4 * 220 + 1
        ],
        "after the edit, upstream site",
    );
    for (i, row) in upstream.iter().enumerate() {
        assert_eq!(
            row.site_id,
            Some(site1_uuid),
            "after the edit: upstream row {i} site"
        );
        assert_eq!(
            row.deployment_id,
            Some(dep1),
            "after the edit: upstream row {i} deployment"
        );
    }

    let downstream = get_readings(&fx.db, stream2_uuid).await;
    check(
        &downstream,
        &[
            ("2025-06-10T02:15:00Z", 230.0, 921.0, c1),  // 4 * 230 + 1
            ("2025-06-10T02:45:00Z", 250.0, 1001.0, c1), // 4 * 250 + 1
        ],
        "after the edit, downstream site",
    );
    for (i, row) in downstream.iter().enumerate() {
        assert_eq!(
            row.site_id,
            Some(site2_uuid),
            "after the edit: downstream row {i} site"
        );
        assert_eq!(
            row.deployment_id,
            Some(dep2),
            "after the edit: downstream row {i} deployment"
        );
    }

    // No manual refresh here: the job's own refresh is the behaviour under test, and it is
    // time-scoped rather than site-scoped, so one edit must reach both sites' buckets.
    assert_bucket(
        hourly(&fx, &site1, "2025-06-10T00:00:00Z", "after the edit").await,
        861.0, // (841 + 881) / 2
        2,
        "the upstream aggregate over the edited window reports the recalculated mean",
    );
    assert_bucket(
        hourly(&fx, &site2, "2025-06-10T02:00:00Z", "after the edit").await,
        961.0, // (921 + 1001) / 2
        2,
        "the downstream aggregate reports it too, so the refresh is not confined to one site",
    );
    assert_bucket(
        hourly(&fx, &site1, "2025-06-05T09:00:00Z", "after the edit").await,
        1025.0,
        2,
        "the bucket inside the preceding curve's window recomputes to the same mean",
    );
}
