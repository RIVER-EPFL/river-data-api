//! A sensor is moved from one site to another, the move date is corrected after the fact (first
//! backwards, then forwards in a separate journey), and finally the downstream deployment is
//! deleted outright.
//!
//! Scenario: an operator mis-records when an instrument was carried downstream and fixes it later.
//! Expected behaviour: attribution is re-derived from the deployment timeline by half-open window,
//! and the single time-scoped continuous-aggregate refresh corrects BOTH the site that loses
//! readings and the site that gains them. One calibration window spans every deployment here, so
//! `calibrated_value` and `calibration_id` are invariant across all four journeys: that invariance
//! is the load-bearing negative assertion that the deployment join and the calibration join are
//! independent.
//!
//! The fixture is Track B (`tests/common/tracks.rs`) extended with a second site, a second
//! site_parameter, and a real calibration entered after the data landed. Raw values are synthetic
//! so every expected bucket mean is exact:
//!
//! ```text
//! calibration C: y = 2x + 5, valid_from 00:00, covering the whole day
//!
//!   bucket 08:00   08:15 raw 10 -> 25    08:45 raw 20 -> 45                        mean  35.0  n 2
//!   bucket 09:00   09:00 raw 30 -> 65    09:15 raw 40 -> 85
//!                  09:30 raw 50 -> 105   09:45 raw 60 -> 125                       mean  95.0  n 4
//!   bucket 10:00   10:15 raw 70 -> 145   10:45 raw 80 -> 165                       mean 155.0  n 2
//! ```
//!
//! Every fixture timestamp is in the past on purpose: the refresh window is `[since, NOW()]`
//! (`common/sync_state.rs`), so a future-dated fixture is never materialised.
//!
//! These run as real Keycloak users, each step as the level that owns it: provisioning is
//! Administrator work, moving an instrument is MANAGER work (`sensor_crud` resolves
//! `Capability::ManageSensors`), ingestion is RIVER work. They self-skip when Keycloak is
//! unreachable unless `REQUIRE_KEYCLOAK` is set.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::e2e;
use crate::common::keycloak as kc;
use crate::common::sensor_lifecycle as sl;
use crate::common::tracks;

/// Reading times (on Track B's base day) and their raw values, in stream order.
const READINGS: [(&str, f64); 8] = [
    ("08:15:00", 10.0),
    ("08:45:00", 20.0),
    ("09:00:00", 30.0),
    ("09:15:00", 40.0),
    ("09:30:00", 50.0),
    ("09:45:00", 60.0),
    ("10:15:00", 70.0),
    ("10:45:00", 80.0),
];

/// The one calibration covering every reading in this file.
const SLOPE: f64 = 2.0;
const INTERCEPT: f64 = 5.0;

fn curve(raw: f64) -> f64 {
    SLOPE * raw + INTERCEPT
}

fn at(hms: &str) -> String {
    format!("{}T{hms}Z", tracks::FLOW_BASE_DAY)
}

fn ts(hms: &str) -> DateTime<Utc> {
    sl::dt(&at(hms))
}

fn uuid(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap_or_else(|e| panic!("expected a uuid, got '{s}': {e}"))
}

struct TwoSites {
    app: axum::Router,
    manager: String,
    river: String,
    sensor: Uuid,
    parameter_id: String,
    site1: String,
    site2: String,
    site1_id: Uuid,
    site2_id: Uuid,
    dep_a: Uuid,
    calibration: Uuid,
    stream: Uuid,
}

/// Track B, plus a downstream site and a real calibration, provisioned entirely over HTTP.
///
/// The readings land identity-calibrated (`/ingest` writes `calibrated_value = raw_value`) and the
/// curve is entered afterwards, which is the ordinary order of events for a field campaign: the
/// `calibration_create` hook reprocesses the history the new window covers. Asserting the resulting
/// buckets here is the anchor every test below measures its transition against.
async fn two_site_fixture(db: &DatabaseConnection) -> TwoSites {
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_sensor_flow_track(&app, &admin).await;
    let parameter_id = track.parameter_id("TrkFlowDO").to_string();
    let sensor = uuid(
        track
            .sensor_id
            .as_deref()
            .expect("Track B provisions a sensor"),
    );
    let dep_a = uuid(
        track
            .deployment_id
            .as_deref()
            .expect("Track B opens a deployment at the upstream site"),
    );
    let stream = uuid(&track.stream_ids[0]);
    let site1 = track.site_id.clone();

    kc::ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    kc::grant_project(
        db,
        &kc::keycloak_user_id("manager1").await,
        &track.project_id,
    )
    .await;
    kc::grant_project(db, &kc::keycloak_user_id("river1").await, &track.project_id).await;
    let manager = kc::get_keycloak_jwt("manager1", "manager1").await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;

    // Without the link, pairing leaves the minted sensor undeployed (the slot is already held) and
    // none of the calibration or deployment work below reaches the readings.
    e2e::link_stream_sensor(&app, &admin, &stream.to_string(), &sensor.to_string()).await;

    let site2 = e2e::create_site(
        &app,
        &admin,
        &track.project_id,
        "Site flow downstream",
        "site-flow-down",
    )
    .await;
    e2e::assign_site_parameter_minimal(&app, &manager, &site2, &parameter_id).await;

    let (status, paired) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{stream}/pair"),
        &json!({ "site_parameter_id": track.site_parameter_ids[0] }),
        &admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "pair the stream to the upstream slot ({status}): {paired}"
    );

    let payload: Vec<serde_json::Value> = READINGS
        .iter()
        .map(|(hms, raw)| json!({ "time": at(hms), "raw_value": raw }))
        .collect();
    let (status, ingested) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &json!({ "stream_id": stream, "readings": payload }),
        &river,
    )
    .await;
    assert_eq!(
        status, 200,
        "ingest the day's readings ({status}): {ingested}"
    );
    assert_eq!(
        ingested["inserted"],
        READINGS.len(),
        "every reading lands: {ingested}"
    );
    assert_eq!(
        ingested["paired"], true,
        "the stream is paired, so the readings arrive attributed: {ingested}"
    );

    let (status, cal) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sensor_calibrations",
        &json!({
            "sensor_id": sensor,
            "parameter_id": parameter_id,
            "slope": SLOPE,
            "intercept": INTERCEPT,
            "valid_from": at("00:00:00"),
        }),
        &manager,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "enter the calibration for the day just logged ({status}): {cal}"
    );
    let calibration = uuid(&e2e::id_of(&cal));

    assert!(
        sl::wait_for_reprocessing(db, sensor, Duration::from_secs(30)).await,
        "entering a calibration reprocesses the history its window covers"
    );

    // The only manual refresh in this file. Everything after a deployment change must come from
    // the job's own refresh.
    e2e::refresh_hourly(db, ts("00:00:00")).await;

    let site1_id = uuid(&site1);
    let site2_id = uuid(&site2);

    assert_bucket(
        e2e::hourly_bucket(db, &site1, &parameter_id, ts("08:00:00")).await,
        35.0,
        2,
        "baseline upstream 08:00 (the curve was applied to the logged history, not left at raw)",
    );
    assert_bucket(
        e2e::hourly_bucket(db, &site1, &parameter_id, ts("09:00:00")).await,
        95.0,
        4,
        "baseline upstream 09:00, the bucket the move will straddle",
    );
    assert_bucket(
        e2e::hourly_bucket(db, &site1, &parameter_id, ts("10:00:00")).await,
        155.0,
        2,
        "baseline upstream 10:00",
    );
    assert!(
        e2e::hourly_bucket(db, &site2, &parameter_id, ts("09:00:00"))
            .await
            .is_none(),
        "the downstream site holds nothing yet, so a later downstream bucket cannot be satisfied \
         by rows that were already there"
    );

    TwoSites {
        app,
        manager,
        river,
        sensor,
        parameter_id,
        site1,
        site2,
        site1_id,
        site2_id,
        dep_a,
        calibration,
        stream,
    }
}

impl TwoSites {
    /// Move the instrument downstream at `hms`, as the MANAGER who owns sensor movement.
    async fn move_downstream(&self, db: &DatabaseConnection, hms: &str) -> Uuid {
        let dep_b = e2e::create_deployment(
            &self.app,
            &self.manager,
            &self.sensor.to_string(),
            &self.site2,
            &self.parameter_id,
            &at(hms),
        )
        .await;
        assert!(
            e2e::wait_for_jobs_by_trigger(db, "deployment_create", 30).await,
            "the move propagates through an observable tracked deployment_create job"
        );
        assert!(
            sl::wait_for_reprocessing(db, self.sensor, Duration::from_secs(30)).await,
            "every reprocessing job for this instrument settles without failing"
        );
        uuid(&dep_b)
    }

    /// Correct a deployment's start date after the fact, the way the deployment editor does.
    async fn correct_start(&self, db: &DatabaseConnection, deployment: Uuid, hms: &str) {
        let (status, body) = crate::common::put_json_with_token(
            &self.app,
            &format!("/api/sensor_deployments/{deployment}"),
            &json!({ "deployed_from": at(hms) }),
            &self.manager,
        )
        .await;
        assert!(
            (200..300).contains(&status),
            "correct the move date to {hms} ({status}): {body}"
        );
        assert!(
            e2e::wait_for_jobs_by_trigger(db, "deployment_update", 30).await,
            "the correction propagates through a tracked deployment_update job"
        );
        assert!(
            sl::wait_for_reprocessing(db, self.sensor, Duration::from_secs(30)).await,
            "every reprocessing job for this instrument settles without failing"
        );
    }

    /// The buckets a move at 09:30 produces, asserted before each later correction so the
    /// transition that correction causes is a real change and not an empty start.
    async fn assert_post_move_buckets(&self, db: &DatabaseConnection) {
        assert_bucket(
            e2e::hourly_bucket(db, &self.site1, &self.parameter_id, ts("08:00:00")).await,
            35.0,
            2,
            "upstream 08:00 sits entirely before the move",
        );
        assert_bucket(
            e2e::hourly_bucket(db, &self.site1, &self.parameter_id, ts("09:00:00")).await,
            75.0,
            2,
            "upstream 09:00 kept only the two readings before the move",
        );
        assert_bucket(
            e2e::hourly_bucket(db, &self.site2, &self.parameter_id, ts("09:00:00")).await,
            115.0,
            2,
            "downstream 09:00 gained exactly the two readings upstream lost",
        );
        assert_bucket(
            e2e::hourly_bucket(db, &self.site2, &self.parameter_id, ts("10:00:00")).await,
            155.0,
            2,
            "downstream 10:00 holds the whole post-move hour",
        );
        assert!(
            e2e::hourly_bucket(db, &self.site1, &self.parameter_id, ts("10:00:00"))
                .await
                .is_none(),
            "upstream 10:00 is gone, not merely reduced"
        );
    }
}

/// Attribution plus the two columns a deployment change must NOT touch. Asserting the calibration
/// join on every row is what makes an indiscriminate rewrite fail here instead of passing.
#[track_caller]
fn assert_attribution(
    row: &sl::ReadingRow,
    site: Option<Uuid>,
    deployment: Option<Uuid>,
    calibrated: f64,
    calibration: Uuid,
    label: &str,
) {
    assert_eq!(row.site_id, site, "{label}: site_id");
    assert_eq!(row.deployment_id, deployment, "{label}: deployment_id");
    assert_eq!(
        row.calibrated_value,
        Some(calibrated),
        "{label}: calibrated_value is the one curve applied to raw {}",
        row.raw_value
    );
    assert_eq!(
        row.calibration_id,
        Some(calibration),
        "{label}: the calibration join is independent of the deployment join"
    );
}

#[track_caller]
fn assert_bucket(actual: Option<(f64, i64)>, mean: f64, count: i64, what: &str) {
    let (actual_mean, actual_count) = actual.unwrap_or_else(|| {
        panic!("{what}: expected mean {mean} over {count} readings, found no bucket at all")
    });
    assert_eq!(
        actual_count, count,
        "{what}: readings counted in the bucket"
    );
    // Every expected mean is an exact IEEE754 value (integer sums over integer counts); the
    // tolerance only absorbs the aggregate's summation order.
    assert!(
        (actual_mean - mean).abs() < 1e-9,
        "{what}: expected mean {mean}, got {actual_mean}"
    );
}

async fn deployment_window(
    db: &DatabaseConnection,
    deployment: Uuid,
) -> (DateTime<Utc>, Option<DateTime<Utc>>) {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT deployed_from, deployed_until FROM sensor_deployments WHERE id = $1",
            [deployment.into()],
        ))
        .await
        .expect("query sensor_deployments")
        .unwrap_or_else(|| panic!("deployment {deployment} row"));
    let from: DateTime<chrono::FixedOffset> =
        row.try_get("", "deployed_from").expect("deployed_from");
    let until: Option<DateTime<chrono::FixedOffset>> =
        row.try_get("", "deployed_until").expect("deployed_until");
    (
        from.with_timezone(&Utc),
        until.map(|t| t.with_timezone(&Utc)),
    )
}

async fn deployment_exists(db: &DatabaseConnection, deployment: Uuid) -> bool {
    db.query_one(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT 1 AS one FROM sensor_deployments WHERE id = $1",
        [deployment.into()],
    ))
    .await
    .expect("query sensor_deployments")
    .is_some()
}

/// One `(parameter, sensor)` series from the uncached `split_by_sensor=true` aggregate read.
async fn split_series(
    app: &axum::Router,
    jwt: &str,
    site: &str,
    what: &str,
) -> (Vec<DateTime<Utc>>, serde_json::Value) {
    let (status, body) = crate::common::get_json_with_token(
        app,
        &format!(
            "/api/sites/{site}/aggregates/hourly?start={}&end={}&split_by_sensor=true",
            at("08:00:00"),
            at("11:00:00")
        ),
        jwt,
    )
    .await;
    assert_eq!(status, 200, "{what} ({status}): {body}");
    let series = body["parameters"]
        .as_array()
        .unwrap_or_else(|| panic!("{what}: no parameters array in {body}"));
    assert_eq!(
        series.len(),
        1,
        "{what}: one instrument feeds this slot, so one series is served: {body}"
    );
    let times: Vec<DateTime<Utc>> = body["times"]
        .as_array()
        .unwrap_or_else(|| panic!("{what}: no times array in {body}"))
        .iter()
        .map(|t| {
            DateTime::parse_from_rfc3339(t.as_str().unwrap_or_default())
                .unwrap_or_else(|e| panic!("{what}: unparseable bucket time {t}: {e}"))
                .with_timezone(&Utc)
        })
        .collect();
    (times, body)
}

#[tokio::test]
#[serial]
async fn moving_a_sensor_moves_its_readings_and_both_sites_aggregates() {
    if !kc::require_keycloak_or_skip("deployment_move_two_sites").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let f = two_site_fixture(&db).await;

    let (status, body) = crate::common::post_json_with_token(
        &f.app,
        "/api/sensor_deployments",
        &json!({
            "sensor_id": f.sensor,
            "site_id": f.site2,
            "parameter_id": f.parameter_id,
            "deployed_from": at("09:30:00"),
        }),
        &f.river,
    )
    .await;
    assert_eq!(
        status, 403,
        "moving an instrument is MANAGER work, a RIVER member is refused: {body}"
    );

    let dep_b = f.move_downstream(&db, "09:30:00").await;

    assert_eq!(
        deployment_window(&db, f.dep_a).await.1,
        Some(ts("09:30:00")),
        "the upstream deployment auto-closes at the move instant"
    );
    assert_eq!(
        deployment_window(&db, dep_b).await.1,
        None,
        "the downstream deployment is open-ended"
    );

    let rows = sl::get_readings(&db, f.stream).await;
    assert_eq!(
        rows.len(),
        READINGS.len(),
        "a move re-attributes readings, it never drops them"
    );
    for (i, row) in rows.iter().enumerate() {
        let (hms, raw) = READINGS[i];
        // The deployment join is half-open at deployed_from, so 09:30 itself belongs downstream.
        let downstream = i >= 4;
        assert_eq!(
            row.raw_value, raw,
            "reading {hms}: the measurement itself is untouched"
        );
        assert_attribution(
            row,
            Some(if downstream { f.site2_id } else { f.site1_id }),
            Some(if downstream { dep_b } else { f.dep_a }),
            curve(raw),
            f.calibration,
            &format!("reading {hms}"),
        );
    }

    // No manual refresh happens after the move: a stale or missing bucket below is a real failure
    // of the propagation contract, not a test artefact. The one refresh the job issues is
    // time-scoped with no site predicate, which is what lets it correct both sites at once.
    f.assert_post_move_buckets(&db).await;

    // Read back through the API surface the dashboard uses. `split_by_sensor=true` also pins which
    // instrument the relocated series belongs to. Each URL is issued exactly once in this test so
    // the response cache cannot mask the transition (its key omits split_by_sensor).
    let (times, body) = split_series(
        &f.app,
        &f.river,
        &f.site1,
        "upstream aggregates after the move",
    )
    .await;
    assert_eq!(
        times,
        vec![ts("08:00:00"), ts("09:00:00")],
        "upstream serves two buckets: {body}"
    );
    assert_eq!(
        body["parameters"][0]["sensor_id"].as_str().map(uuid),
        Some(f.sensor),
        "the upstream series carries the instrument that logged it: {body}"
    );
    assert_eq!(
        e2e::field_for(&body, "TrkFlowDO", "avg"),
        vec![35.0, 75.0],
        "upstream means served over HTTP: {body}"
    );

    let (times, body) = split_series(
        &f.app,
        &f.river,
        &f.site2,
        "downstream aggregates after the move",
    )
    .await;
    assert_eq!(
        times,
        vec![ts("09:00:00"), ts("10:00:00")],
        "downstream serves two buckets: {body}"
    );
    assert_eq!(
        e2e::field_for(&body, "TrkFlowDO", "avg"),
        vec![115.0, 155.0],
        "downstream means served over HTTP: {body}"
    );
}

#[tokio::test]
#[serial]
async fn backdating_the_move_pulls_the_earlier_reading_downstream() {
    if !kc::require_keycloak_or_skip("deployment_backdate_two_sites").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let f = two_site_fixture(&db).await;

    let dep_b = f.move_downstream(&db, "09:30:00").await;
    f.assert_post_move_buckets(&db).await;

    // The instrument actually travelled a quarter of an hour earlier.
    f.correct_start(&db, dep_b, "09:15:00").await;

    assert_eq!(
        deployment_window(&db, f.dep_a).await.1,
        Some(ts("09:15:00")),
        "pulling the later deployment's start back re-chains the earlier deployment's end"
    );
    assert_eq!(
        deployment_window(&db, dep_b).await,
        (ts("09:15:00"), None),
        "the corrected deployment starts at the corrected instant and stays open"
    );

    let rows = sl::get_readings(&db, f.stream).await;
    assert_eq!(
        rows.len(),
        READINGS.len(),
        "correcting a date re-attributes readings, it never drops them"
    );
    for (i, row) in rows.iter().enumerate() {
        let (hms, raw) = READINGS[i];
        let downstream = i >= 3;
        assert_eq!(
            row.raw_value, raw,
            "reading {hms}: the measurement itself is untouched"
        );
        assert_attribution(
            row,
            Some(if downstream { f.site2_id } else { f.site1_id }),
            Some(if downstream { dep_b } else { f.dep_a }),
            curve(raw),
            f.calibration,
            &format!("reading {hms}"),
        );
    }

    assert_bucket(
        e2e::hourly_bucket(&db, &f.site1, &f.parameter_id, ts("09:00:00")).await,
        65.0,
        1,
        "upstream 09:00 was 75.0 over 2 and gives up the 09:15 reading",
    );
    assert_bucket(
        e2e::hourly_bucket(&db, &f.site2, &f.parameter_id, ts("09:00:00")).await,
        105.0,
        3,
        "downstream 09:00 was 115.0 over 2 and gains exactly that reading",
    );
    assert_bucket(
        e2e::hourly_bucket(&db, &f.site1, &f.parameter_id, ts("08:00:00")).await,
        35.0,
        2,
        "upstream 08:00 is untouched by a correction two hours later",
    );
    assert_bucket(
        e2e::hourly_bucket(&db, &f.site2, &f.parameter_id, ts("10:00:00")).await,
        155.0,
        2,
        "downstream 10:00 is untouched",
    );
    assert!(
        e2e::hourly_bucket(&db, &f.site1, &f.parameter_id, ts("10:00:00"))
            .await
            .is_none(),
        "upstream 10:00 stays absent"
    );
}

/// EXPECTED TO FAIL against the current API, and kept failing on purpose.
///
/// `recompute_deployed_until` (`sensors/calibrations/service.rs`) sets
/// `deployed_until = LEAST(existing, LEAD(deployed_from))`, so it can only ever SHORTEN a window.
/// Pushing the downstream start forward therefore does not carry the upstream deployment's end
/// with it: `[09:30, 09:45)` becomes a deployment gap, and the per-sensor recall pass NULL-clears
/// the reading inside it (the slot pass does not, its guard is
/// `time >= MIN(deployed_from) WHERE site_id = downstream`, which now sits after the gap). The
/// reading drops out of both sites' rollups instead of returning upstream.
///
/// The expectation asserted here is the operator-intuitive one, and it is inferred rather than
/// documented: correcting a mistyped move date forward should hand the readings back to where the
/// instrument actually was, exactly as correcting it backwards hands them forward. Closing the gap
/// is a product decision, not a test fix; weakening this test would record the current behaviour as
/// correct forever.
#[tokio::test]
#[serial]
async fn moving_the_move_date_forward_returns_readings_to_the_previous_site() {
    if !kc::require_keycloak_or_skip("deployment_forward_correction").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let f = two_site_fixture(&db).await;

    let dep_b = f.move_downstream(&db, "09:30:00").await;
    f.assert_post_move_buckets(&db).await;

    // The instrument actually travelled a quarter of an hour later.
    f.correct_start(&db, dep_b, "09:45:00").await;

    let rows = sl::get_readings(&db, f.stream).await;
    assert_eq!(
        rows.len(),
        READINGS.len(),
        "correcting a date re-attributes readings, it never drops them"
    );
    for (i, row) in rows.iter().enumerate() {
        let (hms, raw) = READINGS[i];
        let downstream = i >= 5;
        assert_eq!(
            row.raw_value, raw,
            "reading {hms}: the measurement itself is untouched"
        );
        assert_eq!(
            row.sensor_id,
            Some(f.sensor),
            "reading {hms}: the instrument that measured it never changes"
        );
        assert_attribution(
            row,
            Some(if downstream { f.site2_id } else { f.site1_id }),
            Some(if downstream { dep_b } else { f.dep_a }),
            curve(raw),
            f.calibration,
            &format!("reading {hms}"),
        );
    }

    assert_eq!(
        deployment_window(&db, f.dep_a).await.1,
        Some(ts("09:45:00")),
        "the upstream deployment's end follows the corrected move date, leaving no gap"
    );
    assert_eq!(
        deployment_window(&db, dep_b).await,
        (ts("09:45:00"), None),
        "the corrected deployment starts at the corrected instant and stays open"
    );

    assert_bucket(
        e2e::hourly_bucket(&db, &f.site1, &f.parameter_id, ts("09:00:00")).await,
        85.0,
        3,
        "upstream 09:00 was 75.0 over 2 and takes the 09:30 reading back",
    );
    assert_bucket(
        e2e::hourly_bucket(&db, &f.site2, &f.parameter_id, ts("09:00:00")).await,
        125.0,
        1,
        "downstream 09:00 keeps only the reading logged after the corrected move",
    );
    assert_bucket(
        e2e::hourly_bucket(&db, &f.site1, &f.parameter_id, ts("08:00:00")).await,
        35.0,
        2,
        "upstream 08:00 is untouched by a correction an hour later",
    );
    assert_bucket(
        e2e::hourly_bucket(&db, &f.site2, &f.parameter_id, ts("10:00:00")).await,
        155.0,
        2,
        "downstream 10:00 is untouched",
    );
}

#[tokio::test]
#[serial]
async fn deleting_the_downstream_deployment_unattributes_its_readings() {
    if !kc::require_keycloak_or_skip("deployment_delete_two_sites").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let f = two_site_fixture(&db).await;

    let dep_b = f.move_downstream(&db, "09:30:00").await;
    f.assert_post_move_buckets(&db).await;

    let (status, body) = crate::common::delete_with_token(
        &f.app,
        &format!("/api/sensor_deployments/{dep_b}"),
        &f.manager,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "a MANAGER deletes a deployment recorded in error ({status}): {body}"
    );
    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "deployment_delete", 30).await,
        "the delete propagates through a tracked deployment_delete job"
    );
    assert!(
        sl::wait_for_reprocessing(&db, f.sensor, Duration::from_secs(30)).await,
        "every reprocessing job for this instrument settles without failing"
    );

    assert!(
        !deployment_exists(&db, dep_b).await,
        "the deployment row is gone"
    );
    assert_eq!(
        deployment_window(&db, f.dep_a).await.1,
        Some(ts("09:30:00")),
        "the upstream window is not stretched over the vacated period: deployment coverage is \
         gap-preserving"
    );

    let rows = sl::get_readings(&db, f.stream).await;
    assert_eq!(
        rows.len(),
        READINGS.len(),
        "deleting a deployment deletes no readings"
    );
    for (i, row) in rows.iter().enumerate() {
        let (hms, raw) = READINGS[i];
        let orphaned = i >= 4;
        assert_eq!(
            row.raw_value, raw,
            "reading {hms}: the measurement itself is untouched"
        );
        assert_eq!(
            row.sensor_id,
            Some(f.sensor),
            "reading {hms}: the device still measured it, so sensor_id survives un-attribution"
        );
        // `deployment_id = NULL` on the orphaned rows is written synchronously by `perform_delete`
        // before the row is removed, so only `site_id = NULL` is evidence that the tracked job ran.
        assert_attribution(
            row,
            if orphaned { None } else { Some(f.site1_id) },
            if orphaned { None } else { Some(f.dep_a) },
            curve(raw),
            f.calibration,
            &format!("reading {hms}"),
        );
    }

    assert_bucket(
        e2e::hourly_bucket(&db, &f.site1, &f.parameter_id, ts("08:00:00")).await,
        35.0,
        2,
        "upstream 08:00 is bit-identical to the pre-delete baseline",
    );
    assert_bucket(
        e2e::hourly_bucket(&db, &f.site1, &f.parameter_id, ts("09:00:00")).await,
        75.0,
        2,
        "upstream 09:00 is bit-identical to the pre-delete baseline: deleting the downstream \
         deployment must not touch a single upstream reading",
    );
    assert!(
        e2e::hourly_bucket(&db, &f.site2, &f.parameter_id, ts("09:00:00"))
            .await
            .is_none(),
        "the un-attributed readings drop out of the downstream rollup entirely (the aggregate \
         filters on site_id IS NOT NULL), and this bucket was asserted present before the delete"
    );
    assert!(
        e2e::hourly_bucket(&db, &f.site2, &f.parameter_id, ts("10:00:00"))
            .await
            .is_none(),
        "so does the whole downstream 10:00 bucket"
    );
}
