//! Continuous-aggregate correctness as it is *served*, by `GET /api/sites/{id}/aggregates/{res}`.
//!
//! Scenario: an operator onboards a sensor slot (Track B), lands readings through the real write
//! paths, and then reads the rollups the dashboard charts.
//! Expected behaviour: every bucket the endpoint serves is the exact arithmetic of the eligible
//! readings underneath it, and every change that alters those readings reaches the served bucket.
//!
//! Five suites, and only two of them exercise propagation. The other three pin contracts:
//! `served_aggregates_report_exact_values_at_every_resolution` pins the endpoint arithmetic,
//! `continuous_aggregates_apply_their_filter_algebra_exactly` pins the view's WHERE clause, and
//! `editing_a_stored_raw_value_reaches_the_served_aggregate` pins a known gap. The propagation
//! guards are the calibration edit across two sites and the flag-range window.
//!
//! Overlap, deliberately kept: `tests/sensor_calibrations/reprocessing.rs` already covers a
//! calibration *create* reaching one site's hourly rollup, read by SQL. What is added here is an
//! *update* to an existing curve, one window spanning two deployments at two sites, read through
//! HTTP, with an unaffected sensor asserted at reading level so an indiscriminate rewrite fails.
//!
//! Fixture values are synthetic (slope 2.0, intercept 5.0, raw 10.0, so 25.0) so every expectation
//! is checkable by eye and exactly representable in f64. Every fixture timestamp is in the past:
//! the refresh window is `[since, NOW()]`, so a future-dated fixture is silently never
//! materialised. Each suite owns a distinct month so no two suites materialise the same bucket.
//!
//! These run as real Keycloak users, each step at the lowest level that should be able to perform
//! it, with the level below asserted refused. They self-skip when Keycloak is unreachable unless
//! `REQUIRE_KEYCLOAK` is set.

use std::time::Duration;

use axum::Router;
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::{Value, json};
use serial_test::serial;
use uuid::Uuid;

use crate::common::e2e;
use crate::common::keycloak as kc;
use crate::common::sensor_lifecycle as sl;
use crate::common::tracks;

// ============================================================================
// Reading the served response
// ============================================================================

/// One bucket of one parameter series, as the endpoint reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Bucket {
    avg: Option<f64>,
    min: Option<f64>,
    max: Option<f64>,
    count: i64,
    flagged: i64,
}

fn parse_time(ts: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(ts)
        .unwrap_or_else(|e| panic!("invalid timestamp '{ts}': {e}"))
        .with_timezone(&Utc)
}

fn series_of<'a>(body: &'a Value, parameter_id: &str) -> Option<&'a Value> {
    body["parameters"]
        .as_array()?
        .iter()
        .find(|p| p["parameter_id"] == parameter_id)
}

/// The bucket a parameter holds at `at`, or `None` when the series is absent OR the bucket
/// timestamp is not in `times`. Callers asserting absence must assert this `None` explicitly; an
/// `if let Some(..)` shape would skip the assertion in exactly the case being tested.
fn bucket_at(body: &Value, parameter_id: &str, at: &str) -> Option<Bucket> {
    let want = parse_time(at);
    let index = body["times"]
        .as_array()?
        .iter()
        .position(|t| t.as_str().is_some_and(|s| parse_time(s) == want))?;
    let series = series_of(body, parameter_id)?;
    Some(Bucket {
        avg: series["avg"][index].as_f64(),
        min: series["min"][index].as_f64(),
        max: series["max"][index].as_f64(),
        count: series["count"][index].as_i64()?,
        flagged: series["flagged_count"][index].as_i64()?,
    })
}

/// [`bucket_at`] for the buckets a test requires to exist, with the failure named.
fn bucket(body: &Value, parameter_id: &str, at: &str, what: &str) -> Bucket {
    assert!(
        series_of(body, parameter_id).is_some(),
        "{what}: no series for parameter {parameter_id}: {body}"
    );
    bucket_at(body, parameter_id, at).unwrap_or_else(|| {
        panic!("{what}: bucket {at} absent for parameter {parameter_id}: {body}")
    })
}

/// Guards every subsequent lookup: an empty response would make bucket assertions unreachable
/// rather than failing.
fn assert_populated(body: &Value, what: &str) {
    let times = body["times"]
        .as_array()
        .unwrap_or_else(|| panic!("{what}: no times array: {body}"));
    assert!(
        !times.is_empty(),
        "{what}: the response holds no buckets: {body}"
    );
    let parameters = body["parameters"]
        .as_array()
        .unwrap_or_else(|| panic!("{what}: no parameters array: {body}"));
    assert!(
        !parameters.is_empty(),
        "{what}: no parameter series returned: {body}"
    );
}

fn times_len(body: &Value) -> usize {
    body["times"].as_array().map_or(0, Vec::len)
}

// ============================================================================
// HTTP helpers
// ============================================================================

async fn aggregates(
    app: &Router,
    jwt: &str,
    site_id: &str,
    resolution: &str,
    start: &str,
    end: &str,
) -> Value {
    let uri = format!("/api/sites/{site_id}/aggregates/{resolution}?start={start}&end={end}");
    let (status, body) = crate::common::get_json_with_token(app, &uri, jwt).await;
    assert_eq!(status, 200, "GET {uri} ({status}): {body}");
    body
}

fn reading(site_id: &str, parameter_id: &str, time: &str, raw: f64) -> Value {
    json!({
        "site_id": site_id,
        "parameter_id": parameter_id,
        "time": time,
        "raw_value": raw,
        "calibrated_value": null,
    })
}

/// A reading carrying its own calibrated value, so `COALESCE(calibrated_value, raw_value)` has
/// something to prefer.
fn calibrated_reading(
    site_id: &str,
    parameter_id: &str,
    time: &str,
    raw: f64,
    calibrated: f64,
) -> Value {
    json!({
        "site_id": site_id,
        "parameter_id": parameter_id,
        "time": time,
        "raw_value": raw,
        "calibrated_value": calibrated,
    })
}

async fn write_readings(app: &Router, jwt: &str, rows: Vec<Value>) {
    let expected = rows.len() as u64;
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/readings/batch",
        &json!({ "readings": rows }),
        jwt,
    )
    .await;
    assert_eq!(status, 200, "POST /api/readings/batch ({status}): {body}");
    assert_eq!(
        body["inserted"].as_u64(),
        Some(expected),
        "every submitted reading lands: {body}"
    );
}

// ============================================================================
// Database-side helpers (verification only, never provisioning)
// ============================================================================

/// Materialise all four views over one window. `refresh_continuous_aggregate` cannot run inside a
/// transaction, so this goes through the autocommit `exec`. The window must span at least one
/// monthly bucket for the monthly view to accept it.
async fn refresh_views(db: &DatabaseConnection, from: &str, to: &str) {
    for view in [
        "readings_hourly",
        "readings_daily",
        "readings_weekly",
        "readings_monthly",
    ] {
        crate::common::exec(
            db,
            &format!(
                "CALL refresh_continuous_aggregate('{view}', '{from}'::timestamptz, '{to}'::timestamptz)"
            ),
        )
        .await;
    }
}

/// What the hourly rollup would hold if it were refreshed right now, computed off `readings` with
/// the view's own predicate. Lets a test assert a served bucket is *stale* rather than merely
/// unchanged, which is the difference between a real assertion and one that passes when the
/// endpoint is broken.
async fn live_hourly(
    db: &DatabaseConnection,
    site_id: &str,
    parameter_id: &str,
    at: &str,
) -> (Option<f64>, i64) {
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT AVG(COALESCE(calibrated_value, raw_value)) AS mean, COUNT(*)::bigint AS n \
                 FROM readings \
                 WHERE site_id = '{site_id}' AND parameter_id = '{parameter_id}' \
                   AND time_bucket('1 hour', time) = time_bucket('1 hour', '{at}'::timestamptz) \
                   AND replicate_index = 0 \
                   AND is_flagged IS NOT TRUE \
                   AND measurement_type IS DISTINCT FROM 'spot'"
            ),
        ))
        .await
        .expect("live hourly query")
        .expect("aggregate row");
    let mean: Option<f64> = row.try_get("", "mean").ok().flatten();
    let n: i64 = row.try_get("", "n").expect("count column");
    (mean, n)
}

async fn count_rows(db: &DatabaseConnection, predicate: &str) -> i64 {
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT COUNT(*)::bigint AS c FROM readings WHERE {predicate}"),
        ))
        .await
        .expect("count query")
        .expect("count row");
    row.try_get("", "c").expect("count column")
}

/// `(raw_value, is_flagged)` for one slot's readings on one day, ordered by time. `is_flagged` has
/// no per-reading HTTP surface, so flag state is read here.
async fn flag_state(
    db: &DatabaseConnection,
    site_id: &str,
    parameter_id: &str,
    day: &str,
) -> Vec<(f64, bool)> {
    let rows = db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT raw_value, COALESCE(is_flagged, false) AS flagged FROM readings \
                 WHERE site_id = '{site_id}' AND parameter_id = '{parameter_id}' \
                   AND time >= '{day}T00:00:00Z'::timestamptz \
                   AND time < '{day}T00:00:00Z'::timestamptz + INTERVAL '1 day' \
                 ORDER BY time"
            ),
        ))
        .await
        .expect("flag state query");
    rows.iter()
        .map(|r| {
            (
                r.try_get::<f64>("", "raw_value").expect("raw_value"),
                r.try_get::<bool>("", "flagged").expect("flagged"),
            )
        })
        .collect()
}

/// Synchronisation, not an assertion: provisioning enqueues tracked jobs, and a baseline read taken
/// while one is still running would race it.
async fn jobs_settled(db: &DatabaseConnection, timeout_secs: u64) -> bool {
    let start = std::time::Instant::now();
    loop {
        let row = db
            .query_one(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT COUNT(*)::bigint AS active FROM reprocessing_jobs \
                 WHERE status IN ('queued', 'pending', 'running', 'retrying')"
                    .to_string(),
            ))
            .await
            .expect("job status query")
            .expect("count row");
        let active: i64 = row.try_get("", "active").expect("active column");
        if active == 0 {
            return true;
        }
        if start.elapsed().as_secs() > timeout_secs {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ============================================================================
// Onboarding
// ============================================================================

/// Track B onboarded from nothing by an administrator: project, site, parameter, site parameter,
/// sensor, deployment and a registered stream, all through HTTP.
async fn onboard(test_name: &str) -> Option<(DatabaseConnection, Router, String, tracks::Track)> {
    if !kc::require_keycloak_or_skip(test_name).await {
        return None;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    let track = tracks::onboard_sensor_flow_track(&app, &admin).await;
    Some((db, app, admin, track))
}

/// A realm user at one level, granted visibility of the track's project. Fixture users use their
/// username as password.
async fn member(db: &DatabaseConnection, project_id: &str, username: &str, role: &str) -> String {
    kc::ensure_realm_user(username, username, &[role]).await;
    kc::grant_project(db, &kc::keycloak_user_id(username).await, project_id).await;
    kc::get_keycloak_jwt(username, username).await
}

const MANAGER: (&str, &str) = ("manager1", "riverdata-manager");
const RIVER: (&str, &str) = ("river1", "riverdata-river");
const INTERN: (&str, &str) = ("intern1", "riverdata-intern");

// ============================================================================
// The endpoint's arithmetic, at every resolution
// ============================================================================

/// September 2025. 2025-09-01 is a Monday, which is where `time_bucket`'s weekly origin puts a week
/// boundary, so 09-03 and 09-04 fall in the 09-01 week and 09-13 in the 09-08 week.
#[tokio::test]
#[serial]
async fn served_aggregates_report_exact_values_at_every_resolution() {
    let Some((db, app, admin, track)) = onboard("served_aggregates_exact_values").await else {
        return;
    };
    let river = member(&db, &track.project_id, RIVER.0, RIVER.1).await;
    let intern = member(&db, &track.project_id, INTERN.0, INTERN.1).await;

    let site1 = track.site_id.clone();
    let flow = track.parameter_id("TrkFlowDO").to_string();

    // A second parameter at the same site, and a third with no data at all: both must stay clear of
    // the first's numbers.
    let cond = e2e::create_parameter(
        &app,
        &admin,
        "AggPropCond",
        "Aggregate Propagation Conductivity",
        "uS/cm",
    )
    .await;
    e2e::assign_site_parameter_minimal(&app, &admin, &site1, &cond).await;
    let turb = e2e::create_parameter(
        &app,
        &admin,
        "AggPropTurb",
        "Aggregate Propagation Turbidity",
        "NTU",
    )
    .await;
    e2e::assign_site_parameter_minimal(&app, &admin, &site1, &turb).await;

    // A second site carrying the same global parameter: its values must not enter site 1's mean.
    let site2 = e2e::create_site(
        &app,
        &admin,
        &track.project_id,
        "Aggregate Propagation Neighbour",
        "aggprop-neighbour",
    )
    .await;
    e2e::assign_site_parameter_minimal(&app, &admin, &site2, &flow).await;

    // Provisioning enqueues reprocessing; let it finish before any reading exists, so nothing can
    // rewrite a fixture value behind the assertions.
    assert!(
        jobs_settled(&db, 60).await,
        "provisioning jobs settle before the readings land"
    );

    let (status, denied) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/batch",
        &json!({ "readings": [reading(&site1, &flow, "2025-09-03T10:00:00Z", 10.0)] }),
        &intern,
    )
    .await;
    assert_eq!(
        status, 403,
        "landing readings is data curation, above the intern level: {denied}"
    );

    write_readings(
        &app,
        &river,
        vec![
            reading(&site1, &flow, "2025-09-03T10:00:00Z", 10.0),
            reading(&site1, &flow, "2025-09-03T10:30:00Z", 20.0),
            reading(&site1, &flow, "2025-09-03T11:00:00Z", 30.0),
            reading(&site1, &flow, "2025-09-03T11:30:00Z", 50.0),
            reading(&site1, &flow, "2025-09-04T10:00:00Z", 90.0),
            reading(&site1, &flow, "2025-09-13T10:00:00Z", 100.0),
            reading(&site1, &cond, "2025-09-03T10:00:00Z", 7.0),
            reading(&site2, &flow, "2025-09-03T10:00:00Z", 1000.0),
        ],
    )
    .await;

    assert!(
        jobs_settled(&db, 60).await,
        "the write's follow-on jobs settle before the read"
    );
    refresh_views(&db, "2025-08-01", "2025-11-01").await;

    let hourly = aggregates(
        &app,
        &intern,
        &site1,
        "hourly",
        "2025-09-03T00:00:00Z",
        "2025-09-04T23:00:00Z",
    )
    .await;
    assert_populated(&hourly, "hourly");
    assert_eq!(
        times_len(&hourly),
        3,
        "three hours hold readings in this range, and no phantom buckets appear: {hourly}"
    );
    assert_eq!(
        bucket(&hourly, &flow, "2025-09-03T10:00:00Z", "hourly 10:00"),
        Bucket {
            avg: Some(15.0),
            min: Some(10.0),
            max: Some(20.0),
            count: 2,
            flagged: 0
        },
        "hourly 10:00 is the mean of 10 and 20: {hourly}"
    );
    assert_eq!(
        bucket(&hourly, &flow, "2025-09-03T11:00:00Z", "hourly 11:00"),
        Bucket {
            avg: Some(40.0),
            min: Some(30.0),
            max: Some(50.0),
            count: 2,
            flagged: 0
        },
        "hourly 11:00 is the mean of 30 and 50: {hourly}"
    );
    assert_eq!(
        bucket(&hourly, &flow, "2025-09-04T10:00:00Z", "hourly next day"),
        Bucket {
            avg: Some(90.0),
            min: Some(90.0),
            max: Some(90.0),
            count: 1,
            flagged: 0
        },
        "a single-reading hour reports that reading: {hourly}"
    );

    assert_eq!(
        bucket(&hourly, &cond, "2025-09-03T10:00:00Z", "conductivity 10:00"),
        Bucket {
            avg: Some(7.0),
            min: Some(7.0),
            max: Some(7.0),
            count: 1,
            flagged: 0
        },
        "a second parameter at the same site keeps its own value: {hourly}"
    );
    assert_eq!(
        bucket(&hourly, &cond, "2025-09-03T11:00:00Z", "conductivity 11:00"),
        Bucket {
            avg: None,
            min: None,
            max: None,
            count: 0,
            flagged: 0
        },
        "and reports an empty bucket where it has no readings: {hourly}"
    );

    let turbidity = series_of(&hourly, &turb)
        .unwrap_or_else(|| panic!("an active site parameter always yields a series: {hourly}"));
    let turbidity_counts: Vec<i64> = turbidity["count"]
        .as_array()
        .expect("count array")
        .iter()
        .map(|v| v.as_i64().expect("count is an integer"))
        .collect();
    assert_eq!(
        turbidity_counts,
        vec![0, 0, 0],
        "a parameter with no readings reports zero everywhere: {hourly}"
    );
    assert!(
        turbidity["avg"]
            .as_array()
            .expect("avg array")
            .iter()
            .all(Value::is_null),
        "and a null mean everywhere: {hourly}"
    );

    let daily = aggregates(
        &app,
        &intern,
        &site1,
        "daily",
        "2025-09-03T00:00:00Z",
        "2025-09-13T00:00:00Z",
    )
    .await;
    assert_populated(&daily, "daily");
    assert_eq!(times_len(&daily), 3, "three days hold readings: {daily}");
    assert_eq!(
        bucket(&daily, &flow, "2025-09-03T00:00:00Z", "daily 09-03"),
        Bucket {
            avg: Some(27.5),
            min: Some(10.0),
            max: Some(50.0),
            count: 4,
            flagged: 0
        },
        "daily 09-03 is (10+20+30+50)/4: {daily}"
    );
    assert_eq!(
        bucket(&daily, &flow, "2025-09-04T00:00:00Z", "daily 09-04"),
        Bucket {
            avg: Some(90.0),
            min: Some(90.0),
            max: Some(90.0),
            count: 1,
            flagged: 0
        },
        "daily 09-04: {daily}"
    );
    assert_eq!(
        bucket(&daily, &flow, "2025-09-13T00:00:00Z", "daily 09-13"),
        Bucket {
            avg: Some(100.0),
            min: Some(100.0),
            max: Some(100.0),
            count: 1,
            flagged: 0
        },
        "daily 09-13: {daily}"
    );

    let weekly = aggregates(
        &app,
        &intern,
        &site1,
        "weekly",
        "2025-09-01T00:00:00Z",
        "2025-09-08T00:00:00Z",
    )
    .await;
    assert_populated(&weekly, "weekly");
    assert_eq!(
        times_len(&weekly),
        2,
        "two Monday-anchored weeks hold readings: {weekly}"
    );
    assert_eq!(
        bucket(&weekly, &flow, "2025-09-01T00:00:00Z", "week of 09-01"),
        Bucket {
            avg: Some(40.0),
            min: Some(10.0),
            max: Some(90.0),
            count: 5,
            flagged: 0
        },
        "the 09-01 week is (10+20+30+50+90)/5: {weekly}"
    );
    assert_eq!(
        bucket(&weekly, &flow, "2025-09-08T00:00:00Z", "week of 09-08"),
        Bucket {
            avg: Some(100.0),
            min: Some(100.0),
            max: Some(100.0),
            count: 1,
            flagged: 0
        },
        "the 09-08 week holds only 09-13: {weekly}"
    );

    let monthly = aggregates(
        &app,
        &intern,
        &site1,
        "monthly",
        "2025-08-01T00:00:00Z",
        "2025-10-01T00:00:00Z",
    )
    .await;
    assert_populated(&monthly, "monthly");
    assert_eq!(
        times_len(&monthly),
        1,
        "one month holds readings: {monthly}"
    );
    assert_eq!(
        bucket(
            &monthly,
            &flow,
            "2025-09-01T00:00:00Z",
            "month of September"
        ),
        Bucket {
            avg: Some(50.0),
            min: Some(10.0),
            max: Some(100.0),
            count: 6,
            flagged: 0
        },
        "September is (10+20+30+50+90+100)/6: {monthly}"
    );

    let neighbour = aggregates(
        &app,
        &intern,
        &site2,
        "hourly",
        "2025-09-03T00:00:00Z",
        "2025-09-03T23:00:00Z",
    )
    .await;
    assert_populated(&neighbour, "neighbouring site hourly");
    assert_eq!(
        bucket(&neighbour, &flow, "2025-09-03T10:00:00Z", "neighbour 10:00"),
        Bucket {
            avg: Some(1000.0),
            min: Some(1000.0),
            max: Some(1000.0),
            count: 1,
            flagged: 0
        },
        "the neighbouring site reports its own reading: {neighbour}"
    );
    assert_eq!(
        bucket(
            &hourly,
            &flow,
            "2025-09-03T10:00:00Z",
            "site 1 after the neighbour read"
        )
        .avg,
        Some(15.0),
        "and none of its 1000.0 entered site 1's mean: {hourly}"
    );
}

// ============================================================================
// Propagation: one calibration window, two deployments, two sites
// ============================================================================

/// October 2025. The sensor Track B deployed upstream moves downstream at midday under a single
/// calibration window; editing that window's coefficients must move both sites' served buckets,
/// because the post-reprocess aggregate refresh is time-windowed and global, with no site predicate.
#[tokio::test]
#[serial]
async fn calibration_edit_moves_both_deployed_sites_served_aggregates() {
    let Some((db, app, admin, track)) = onboard("calibration_edit_two_sites").await else {
        return;
    };
    let manager = member(&db, &track.project_id, MANAGER.0, MANAGER.1).await;
    let river = member(&db, &track.project_id, RIVER.0, RIVER.1).await;
    let intern = member(&db, &track.project_id, INTERN.0, INTERN.1).await;

    let site1 = track.site_id.clone();
    let flow = track.parameter_id("TrkFlowDO").to_string();
    let sensor_id = track
        .sensor_id
        .clone()
        .expect("Track B provisions a sensor");
    let sensor: Uuid = sensor_id.parse().expect("sensor uuid");

    let (status, cal) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sensor_calibrations",
        &json!({
            "sensor_id": sensor_id,
            "parameter_id": flow,
            "slope": 1.0,
            "intercept": 0.0,
            "valid_from": "2025-10-01T00:00:00Z",
        }),
        &manager,
    )
    .await;
    assert_eq!(
        status, 201,
        "a manager authors the calibration ({status}): {cal}"
    );
    let calibration_id = e2e::id_of(&cal);
    let calibration: Uuid = calibration_id.parse().expect("calibration uuid");

    let site2 = e2e::create_site(
        &app,
        &admin,
        &track.project_id,
        "Aggregate Propagation Downstream",
        "aggprop-down",
    )
    .await;
    e2e::assign_site_parameter_minimal(&app, &admin, &site2, &flow).await;

    // Opening the downstream deployment closes the upstream one at the same instant, so the two
    // windows are gapless and half-open: a reading at exactly the move instant belongs downstream.
    let deployment_b = e2e::create_deployment(
        &app,
        &manager,
        &sensor_id,
        &site2,
        &flow,
        "2025-10-01T12:00:00Z",
    )
    .await;
    let deployment_b_id: Uuid = deployment_b.parse().expect("deployment uuid");

    // The unaffected side: a different sensor on a different slot at the same site, over the same
    // hours, under its own calibration.
    let control_param = e2e::create_parameter(
        &app,
        &admin,
        "AggPropCtl",
        "Aggregate Propagation Control",
        "uS/cm",
    )
    .await;
    e2e::assign_site_parameter_minimal(&app, &admin, &site1, &control_param).await;
    let control_sensor = e2e::create_sensor(&app, &admin, &control_param, "AGGPROP-CTL-0001").await;
    e2e::create_deployment(
        &app,
        &manager,
        &control_sensor,
        &site1,
        &control_param,
        "2025-10-01T00:00:00Z",
    )
    .await;
    let (status, control_cal) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sensor_calibrations",
        &json!({
            "sensor_id": control_sensor,
            "parameter_id": control_param,
            "slope": 1.0,
            "intercept": 0.0,
            "valid_from": "2025-10-01T00:00:00Z",
        }),
        &manager,
    )
    .await;
    assert_eq!(status, 201, "control calibration ({status}): {control_cal}");
    let control_calibration: Uuid = e2e::id_of(&control_cal).parse().expect("calibration uuid");

    // Calibration and deployment hooks each enqueue reprocessing; let those finish while there is
    // nothing to reprocess, so the only rewrite the assertions can see is the one under test.
    assert!(
        jobs_settled(&db, 60).await,
        "provisioning jobs settle before the readings land"
    );

    write_readings(
        &app,
        &river,
        vec![
            calibrated_reading(&site1, &flow, "2025-10-01T02:00:00Z", 10.0, 10.0),
            calibrated_reading(&site1, &flow, "2025-10-01T02:30:00Z", 20.0, 20.0),
            calibrated_reading(&site2, &flow, "2025-10-01T12:00:00Z", 30.0, 30.0),
            calibrated_reading(&site2, &flow, "2025-10-01T14:00:00Z", 40.0, 40.0),
            calibrated_reading(&site2, &flow, "2025-10-01T14:30:00Z", 60.0, 60.0),
            calibrated_reading(&site1, &control_param, "2025-10-01T02:00:00Z", 8.0, 8.0),
            calibrated_reading(&site1, &control_param, "2025-10-01T02:30:00Z", 12.0, 12.0),
        ],
    )
    .await;

    assert!(
        jobs_settled(&db, 60).await,
        "the write's follow-on jobs settle before the baseline"
    );
    refresh_views(&db, "2025-09-01", "2025-12-01").await;

    let site1_uuid: Uuid = site1.parse().expect("site uuid");
    let site2_uuid: Uuid = site2.parse().expect("site uuid");

    let before = sl::get_readings_for_sensor(&db, sensor).await;
    assert_eq!(
        before.len(),
        5,
        "the moved sensor owns exactly its five readings: {before:?}"
    );
    let expected_before = [
        (10.0, 10.0, site1_uuid),
        (20.0, 20.0, site1_uuid),
        (30.0, 30.0, site2_uuid),
        (40.0, 40.0, site2_uuid),
        (60.0, 60.0, site2_uuid),
    ];
    for (i, (raw, cal_value, site)) in expected_before.iter().enumerate() {
        assert_eq!(
            before[i].raw_value, *raw,
            "reading {i} raw value: {before:?}"
        );
        assert_eq!(
            before[i].calibrated_value,
            Some(*cal_value),
            "reading {i} starts on the identity curve: {before:?}"
        );
        assert_eq!(
            before[i].site_id,
            Some(*site),
            "reading {i} site: {before:?}"
        );
        assert_eq!(
            before[i].calibration_id,
            Some(calibration),
            "reading {i} resolves the one calibration window: {before:?}"
        );
    }
    assert_eq!(
        before[2].deployment_id,
        Some(deployment_b_id),
        "the reading at exactly the move instant belongs to the new deployment, the windows being \
         half-open on [from, until): {before:?}"
    );

    let site1_before = aggregates(
        &app,
        &intern,
        &site1,
        "hourly",
        "2025-10-01T00:00:00Z",
        "2025-10-01T23:00:00Z",
    )
    .await;
    let site2_before = aggregates(
        &app,
        &intern,
        &site2,
        "hourly",
        "2025-10-01T00:00:00Z",
        "2025-10-01T23:00:00Z",
    )
    .await;
    assert_populated(&site1_before, "upstream baseline");
    assert_populated(&site2_before, "downstream baseline");

    assert_eq!(
        bucket(
            &site1_before,
            &flow,
            "2025-10-01T02:00:00Z",
            "upstream baseline"
        ),
        Bucket {
            avg: Some(15.0),
            min: Some(10.0),
            max: Some(20.0),
            count: 2,
            flagged: 0
        },
        "upstream starts at the mean of 10 and 20: {site1_before}"
    );
    assert_eq!(
        bucket(
            &site2_before,
            &flow,
            "2025-10-01T12:00:00Z",
            "downstream baseline noon"
        ),
        Bucket {
            avg: Some(30.0),
            min: Some(30.0),
            max: Some(30.0),
            count: 1,
            flagged: 0
        },
        "the move-instant reading rolls up downstream: {site2_before}"
    );
    assert_eq!(
        bucket(
            &site2_before,
            &flow,
            "2025-10-01T14:00:00Z",
            "downstream baseline 14:00"
        ),
        Bucket {
            avg: Some(50.0),
            min: Some(40.0),
            max: Some(60.0),
            count: 2,
            flagged: 0
        },
        "downstream starts at the mean of 40 and 60: {site2_before}"
    );
    assert_eq!(
        bucket(
            &site1_before,
            &control_param,
            "2025-10-01T02:00:00Z",
            "control baseline"
        ),
        Bucket {
            avg: Some(10.0),
            min: Some(8.0),
            max: Some(12.0),
            count: 2,
            flagged: 0
        },
        "the control slot starts at the mean of 8 and 12: {site1_before}"
    );
    assert!(
        bucket_at(&site1_before, &flow, "2025-10-01T14:00:00Z").is_none(),
        "upstream holds no afternoon bucket: {site1_before}"
    );
    assert!(
        bucket_at(&site2_before, &flow, "2025-10-01T02:00:00Z").is_none(),
        "downstream holds no morning bucket: {site2_before}"
    );

    let (status, denied) = crate::common::put_json_with_token(
        &app,
        &format!("/api/sensor_calibrations/{calibration_id}"),
        &json!({ "slope": 2.0, "intercept": 5.0 }),
        &river,
    )
    .await;
    assert_eq!(
        status, 403,
        "editing a calibration is sensor movement, above the river level: {denied}"
    );

    let (status, updated) = crate::common::put_json_with_token(
        &app,
        &format!("/api/sensor_calibrations/{calibration_id}"),
        &json!({ "slope": 2.0, "intercept": 5.0 }),
        &manager,
    )
    .await;
    assert_eq!(
        status, 200,
        "a manager edits the curve ({status}): {updated}"
    );
    assert!(
        sl::wait_for_reprocessing(&db, sensor, Duration::from_secs(120)).await,
        "the calibration edit enqueues a reprocessing job for the sensor, and it succeeds"
    );

    let after = sl::get_readings_for_sensor(&db, sensor).await;
    assert_eq!(after.len(), 5, "no reading appeared or vanished: {after:?}");
    let expected_after = [
        (10.0, 25.0, site1_uuid),
        (20.0, 45.0, site1_uuid),
        (30.0, 65.0, site2_uuid),
        (40.0, 85.0, site2_uuid),
        (60.0, 125.0, site2_uuid),
    ];
    for (i, (raw, cal_value, site)) in expected_after.iter().enumerate() {
        assert_eq!(
            after[i].raw_value, *raw,
            "reading {i} raw value is never rewritten: {after:?}"
        );
        assert_eq!(
            after[i].calibrated_value,
            Some(*cal_value),
            "reading {i} is 2 * raw + 5: {after:?}"
        );
        assert_eq!(
            after[i].site_id,
            Some(*site),
            "reading {i} keeps its site: {after:?}"
        );
        assert_eq!(
            after[i].calibration_id,
            Some(calibration),
            "reading {i} still resolves the same window: {after:?}"
        );
    }

    let control_rows =
        sl::get_readings_for_sensor(&db, control_sensor.parse().expect("control sensor uuid"))
            .await;
    assert_eq!(
        control_rows.len(),
        2,
        "the control sensor owns its two readings: {control_rows:?}"
    );
    for (i, value) in [8.0, 12.0].iter().enumerate() {
        assert_eq!(
            control_rows[i].calibrated_value,
            Some(*value),
            "an unrelated sensor's value is untouched by the edit: {control_rows:?}"
        );
        assert_eq!(
            control_rows[i].calibration_id,
            Some(control_calibration),
            "and still carries its own calibration: {control_rows:?}"
        );
    }

    let site1_after = aggregates(
        &app,
        &intern,
        &site1,
        "hourly",
        "2025-10-01T00:00:00Z",
        "2025-10-01T23:00:00Z",
    )
    .await;
    let site2_after = aggregates(
        &app,
        &intern,
        &site2,
        "hourly",
        "2025-10-01T00:00:00Z",
        "2025-10-01T23:00:00Z",
    )
    .await;
    assert_populated(&site1_after, "upstream after");
    assert_populated(&site2_after, "downstream after");

    assert_eq!(
        bucket(
            &site1_after,
            &flow,
            "2025-10-01T02:00:00Z",
            "upstream after"
        ),
        Bucket {
            avg: Some(35.0),
            min: Some(25.0),
            max: Some(45.0),
            count: 2,
            flagged: 0
        },
        "the served upstream bucket carries the new curve, not merely a completed job: {site1_after}"
    );
    assert_eq!(
        bucket(
            &site2_after,
            &flow,
            "2025-10-01T12:00:00Z",
            "downstream after noon"
        ),
        Bucket {
            avg: Some(65.0),
            min: Some(65.0),
            max: Some(65.0),
            count: 1,
            flagged: 0
        },
        "one calibration window, two deployments, and the second site moves too: {site2_after}"
    );
    assert_eq!(
        bucket(
            &site2_after,
            &flow,
            "2025-10-01T14:00:00Z",
            "downstream after 14:00"
        ),
        Bucket {
            avg: Some(105.0),
            min: Some(85.0),
            max: Some(125.0),
            count: 2,
            flagged: 0
        },
        "the afternoon bucket is the mean of 85 and 125: {site2_after}"
    );

    // The refresh is time-windowed and global, so this bucket was re-materialised; the assertion
    // says its value did not change, not that it was skipped. The reading-level control above is
    // what would catch an indiscriminate rewrite.
    assert_eq!(
        bucket(
            &site1_after,
            &control_param,
            "2025-10-01T02:00:00Z",
            "control after"
        ),
        Bucket {
            avg: Some(10.0),
            min: Some(8.0),
            max: Some(12.0),
            count: 2,
            flagged: 0
        },
        "re-materialising the control slot reproduces its unchanged values: {site1_after}"
    );
    assert!(
        bucket_at(&site1_after, &flow, "2025-10-01T14:00:00Z").is_none(),
        "the reprocess moved no reading upstream: {site1_after}"
    );
    assert!(
        bucket_at(&site2_after, &flow, "2025-10-01T02:00:00Z").is_none(),
        "and none downstream: {site2_after}"
    );
}

// ============================================================================
// The view's WHERE clause, as one exact bucket
// ============================================================================

/// November 2025. Every excluded row carries a value that would visibly wreck the mean if it leaked,
/// so `count == 2` and `avg == 15.0` prove all four exclusions at once.
#[tokio::test]
#[serial]
async fn continuous_aggregates_apply_their_filter_algebra_exactly() {
    let Some((db, app, admin, track)) = onboard("cagg_filter_algebra").await else {
        return;
    };
    let river = member(&db, &track.project_id, RIVER.0, RIVER.1).await;
    let intern = member(&db, &track.project_id, INTERN.0, INTERN.1).await;

    let site1 = track.site_id.clone();
    let flow = track.parameter_id("TrkFlowDO").to_string();

    let derived_param = e2e::create_parameter(
        &app,
        &admin,
        "AggPropDerived",
        "Aggregate Propagation Derived",
        "uM",
    )
    .await;
    e2e::assign_site_parameter_minimal(&app, &admin, &site1, &derived_param).await;

    // Let the onboarding reprocess finish before any reading exists: a later pass would rewrite
    // `calibrated_value` from `raw_value` and destroy the COALESCE probe below.
    assert!(
        jobs_settled(&db, 60).await,
        "provisioning jobs settle before the readings land"
    );

    write_readings(
        &app,
        &river,
        vec![
            reading(&site1, &flow, "2025-11-05T09:00:00Z", 10.0),
            calibrated_reading(&site1, &flow, "2025-11-05T09:10:00Z", 999.0, 20.0),
            reading(&site1, &flow, "2025-11-05T09:30:00Z", 1000.0),
            json!({
                "site_id": site1, "parameter_id": flow, "time": "2025-11-05T09:40:00Z",
                "raw_value": 2000.0, "calibrated_value": null, "replicate_index": 1,
            }),
            json!({
                "site_id": site1, "parameter_id": flow, "time": "2025-11-05T09:50:00Z",
                "raw_value": 3000.0, "calibrated_value": null, "measurement_type": "spot",
            }),
            json!({
                "site_id": site1, "parameter_id": derived_param, "time": "2025-11-05T09:00:00Z",
                "raw_value": 42.0, "calibrated_value": null, "measurement_type": "derived",
            }),
            reading(&site1, &flow, "2025-11-05T10:00:00Z", 100.0),
            reading(&site1, &flow, "2025-11-05T10:30:00Z", 200.0),
        ],
    )
    .await;

    // The unpaired row: Track B's registered stream has no site parameter yet, so its readings land
    // with site_id NULL. A member is confined to their granted projects and an unpaired stream sits
    // in none of them, so only an administrator can write it.
    let unpaired_stream = track.stream_ids[0].clone();
    let unpaired = json!({
        "stream_id": unpaired_stream,
        "readings": [{ "time": "2025-11-05T09:55:00Z", "raw_value": 4000.0 }],
    });
    let (status, denied) =
        crate::common::post_json_with_token(&app, "/api/ingest", &unpaired, &river).await;
    assert_eq!(
        status, 403,
        "a project-confined member cannot ingest into a stream that belongs to no project: {denied}"
    );
    let (status, ingested) =
        crate::common::post_json_parse_with_token(&app, "/api/ingest", &unpaired, &admin).await;
    assert_eq!(
        status, 200,
        "an administrator lands the unpaired row ({status}): {ingested}"
    );
    assert_eq!(
        ingested["inserted"], 1,
        "the unpaired row is stored: {ingested}"
    );
    assert_eq!(
        ingested["paired"], false,
        "and it is stored unattributed: {ingested}"
    );

    let (status, flagged) = crate::common::patch_json_with_token(
        &app,
        "/api/readings/flag",
        &json!({
            "readings": [{ "site_id": site1, "parameter_id": flow, "time": "2025-11-05T09:30:00Z" }],
            "reason": "sensor maintenance",
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "flagging one reading ({status}): {flagged}");
    assert!(
        flagged.contains("\"updated\":1"),
        "exactly one reading is flagged: {flagged}"
    );

    assert!(
        jobs_settled(&db, 60).await,
        "the write's follow-on jobs settle before the read"
    );
    refresh_views(&db, "2025-10-01", "2025-12-15").await;

    let hourly = aggregates(
        &app,
        &intern,
        &site1,
        "hourly",
        "2025-11-05T00:00:00Z",
        "2025-11-05T23:00:00Z",
    )
    .await;
    assert_populated(&hourly, "filter algebra");

    assert_eq!(
        bucket(&hourly, &flow, "2025-11-05T09:00:00Z", "the filtered hour"),
        Bucket {
            avg: Some(15.0),
            min: Some(10.0),
            max: Some(20.0),
            count: 2,
            flagged: 1
        },
        "only the two eligible readings roll up: the flagged 1000, the replicate-1 2000, the spot \
         3000 and the site-less 4000 are all excluded, and COALESCE took the calibrated 20 over the \
         raw 999. The flagged row still shows in the live flagged tally: {hourly}"
    );
    assert_eq!(
        bucket(
            &hourly,
            &derived_param,
            "2025-11-05T09:00:00Z",
            "the derived probe"
        ),
        Bucket {
            avg: Some(42.0),
            min: Some(42.0),
            max: Some(42.0),
            count: 1,
            flagged: 0
        },
        "a derived reading rolls up under its own parameter; only spot is excluded: {hourly}"
    );
    assert_eq!(
        bucket(
            &hourly,
            &derived_param,
            "2025-11-05T10:00:00Z",
            "the derived probe, next hour"
        ),
        Bucket {
            avg: None,
            min: None,
            max: None,
            count: 0,
            flagged: 0
        },
        "and it does not smear into the control hour: {hourly}"
    );
    assert_eq!(
        bucket(&hourly, &flow, "2025-11-05T10:00:00Z", "the control hour"),
        Bucket {
            avg: Some(150.0),
            min: Some(100.0),
            max: Some(200.0),
            count: 2,
            flagged: 0
        },
        "the hour with no special rows is the plain mean of 100 and 200: {hourly}"
    );

    let parameters = hourly["parameters"].as_array().expect("parameters array");
    assert!(
        !parameters.is_empty(),
        "the sweep below needs series to sweep: {hourly}"
    );
    for series in parameters {
        for value in series["max"].as_array().into_iter().flatten() {
            assert_ne!(
                value.as_f64(),
                Some(4000.0),
                "an unattributed reading must not surface in any site series: {hourly}"
            );
        }
    }

    assert_eq!(
        count_rows(&db, "TRUE").await,
        9,
        "the rollup filters readings, it does not delete them"
    );
    assert_eq!(
        count_rows(&db, "raw_value IN (1000, 2000, 3000, 4000)").await,
        4,
        "each excluded reading is still stored with its original value"
    );
}

// ============================================================================
// Known gap: a stored value edited out of band
// ============================================================================

/// December 2025. No HTTP route rewrites an existing reading's `raw_value` outside the sync-only
/// overwrite ingest, so the edit is made at the storage layer, which is the layer the gap lives at.
/// The intended behaviour is that the served bucket reflects the stored data; today nothing
/// refreshes the rollups after such an edit, and `POST /actions/refresh_aggregates {full:true}` is
/// the documented recovery. Both are asserted, so the recovery is proven even while the gap stands.
#[tokio::test]
#[serial]
async fn editing_a_stored_raw_value_reaches_the_served_aggregate() {
    let Some((db, app, admin, track)) = onboard("raw_value_edit_reaches_aggregate").await else {
        return;
    };
    let river = member(&db, &track.project_id, RIVER.0, RIVER.1).await;
    let intern = member(&db, &track.project_id, INTERN.0, INTERN.1).await;

    let site1 = track.site_id.clone();
    let flow = track.parameter_id("TrkFlowDO").to_string();

    // The gap under test is about a value edited after the fact; let onboarding's reprocess finish
    // first so no later pass can restamp `calibrated_value` and mask the edit.
    assert!(
        jobs_settled(&db, 60).await,
        "provisioning jobs settle before the readings land"
    );

    write_readings(
        &app,
        &river,
        vec![
            reading(&site1, &flow, "2025-12-10T08:00:00Z", 10.0),
            reading(&site1, &flow, "2025-12-10T08:30:00Z", 20.0),
            reading(&site1, &flow, "2025-12-10T09:00:00Z", 100.0),
            reading(&site1, &flow, "2025-12-10T09:30:00Z", 200.0),
        ],
    )
    .await;

    assert!(
        jobs_settled(&db, 60).await,
        "the write's follow-on jobs settle before the baseline"
    );
    refresh_views(&db, "2025-11-01", "2026-01-15").await;

    let day = ("2025-12-10T00:00:00Z", "2025-12-10T23:00:00Z");
    let baseline = aggregates(&app, &intern, &site1, "hourly", day.0, day.1).await;
    assert_populated(&baseline, "baseline");
    assert_eq!(
        times_len(&baseline),
        2,
        "two hours hold readings: {baseline}"
    );
    assert_eq!(
        bucket(&baseline, &flow, "2025-12-10T08:00:00Z", "baseline target"),
        Bucket {
            avg: Some(15.0),
            min: Some(10.0),
            max: Some(20.0),
            count: 2,
            flagged: 0
        },
        "the target hour starts at the mean of 10 and 20: {baseline}"
    );
    let control = Bucket {
        avg: Some(150.0),
        min: Some(100.0),
        max: Some(200.0),
        count: 2,
        flagged: 0,
    };
    assert_eq!(
        bucket(&baseline, &flow, "2025-12-10T09:00:00Z", "baseline control"),
        control,
        "the control hour starts at the mean of 100 and 200: {baseline}"
    );

    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET raw_value = 1000.0 \
             WHERE site_id = '{site1}' AND parameter_id = '{flow}' \
               AND time = '2025-12-10T08:00:00Z'::timestamptz"
        ),
    )
    .await;
    assert_eq!(
        live_hourly(&db, &site1, &flow, "2025-12-10T08:00:00Z").await,
        (Some(510.0), 2),
        "the edit landed: the stored readings now average (1000 + 20) / 2"
    );

    let after_edit = aggregates(&app, &intern, &site1, "hourly", day.0, day.1).await;
    assert_populated(&after_edit, "after the edit");
    let served_after_edit = bucket(&after_edit, &flow, "2025-12-10T08:00:00Z", "after the edit");
    assert_eq!(
        bucket(
            &after_edit,
            &flow,
            "2025-12-10T09:00:00Z",
            "control after the edit"
        ),
        control,
        "editing one hour leaves the next alone: {after_edit}"
    );
    assert_eq!(
        times_len(&after_edit),
        2,
        "and invents no buckets: {after_edit}"
    );

    let (status, denied) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/refresh_aggregates",
        &json!({ "full": true }),
        &intern,
    )
    .await;
    assert_eq!(
        status, 403,
        "triggering a refresh is above the intern level: {denied}"
    );

    let (status, queued) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/refresh_aggregates",
        &json!({ "full": true }),
        &river,
    )
    .await;
    assert_eq!(
        status, 200,
        "a river member triggers the full refresh ({status}): {queued}"
    );
    let job_id = queued["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the refresh returns its job id: {queued}"))
        .to_string();
    // Polled as an administrator, not as the river member who triggered it: `inject_read_scope`
    // scopes `reprocessing_jobs` by `sensor_id IN (scoped sensors)`, and a refresh_aggregates job
    // carries a NULL sensor_id, so the member who queued it gets a 404 on its own job. That is a
    // real defect, but it is not this test's subject and it is asserted deliberately in
    // tests/readings/csv_import_sessions.rs; the poll here is a barrier, not a claim about who may
    // read jobs.
    assert_eq!(
        e2e::poll_job(&app, &admin, &job_id, 120).await,
        "completed",
        "the full refresh job finishes"
    );

    let recovered_body = aggregates(&app, &intern, &site1, "hourly", day.0, day.1).await;
    assert_populated(&recovered_body, "after the full refresh");
    let recovered = bucket(
        &recovered_body,
        &flow,
        "2025-12-10T08:00:00Z",
        "after the full refresh",
    );
    assert_eq!(
        recovered,
        Bucket {
            avg: Some(510.0),
            min: Some(20.0),
            max: Some(1000.0),
            count: 2,
            flagged: 0
        },
        "the documented recovery reproduces the stored data exactly: {recovered_body}"
    );
    assert_eq!(
        bucket(
            &recovered_body,
            &flow,
            "2025-12-10T09:00:00Z",
            "control after the full refresh"
        ),
        control,
        "and does not perturb the hour it should not touch: {recovered_body}"
    );
    assert_eq!(
        times_len(&recovered_body),
        2,
        "nor invent buckets: {recovered_body}"
    );

    assert_eq!(
        served_after_edit, recovered,
        "an edit to a stored value must reach the served bucket without an operator asking for it. \
         Straight after the edit the endpoint served {served_after_edit:?} while the stored \
         readings averaged 510.0 over 2 rows, which the full refresh then produced. Known gap: no \
         write path refreshes the rollups after a direct value edit (the alternative explanation, \
         real-time aggregation being enabled on the views, would have made the two agree)."
    );
}

// ============================================================================
// Propagation: flag_range refreshes its own window
// ============================================================================

/// Flagging refreshes `[start - 32 days, end + 32 days]` on all four views. A bucket 83 days away is
/// outside that, and is deliberately made stale beforehand so "unchanged" is a real claim: its
/// stored readings average 250.0 while the rollup still serves 150.0.
#[tokio::test]
#[serial]
async fn flag_range_refreshes_its_own_window_and_leaves_others_alone() {
    let Some((db, app, _admin, track)) = onboard("flag_range_window_scope").await else {
        return;
    };
    let river = member(&db, &track.project_id, RIVER.0, RIVER.1).await;
    let intern = member(&db, &track.project_id, INTERN.0, INTERN.1).await;

    let site1 = track.site_id.clone();
    let flow = track.parameter_id("TrkFlowDO").to_string();

    // As in the raw-value suite: onboarding's reprocess must be done before the fixtures land, or a
    // later pass would restamp `calibrated_value` and hide the deliberate staleness below.
    assert!(
        jobs_settled(&db, 60).await,
        "provisioning jobs settle before the readings land"
    );

    write_readings(
        &app,
        &river,
        vec![
            reading(&site1, &flow, "2026-02-01T08:00:00Z", 100.0),
            reading(&site1, &flow, "2026-02-01T08:30:00Z", 200.0),
            reading(&site1, &flow, "2026-04-25T08:00:00Z", 10.0),
            reading(&site1, &flow, "2026-04-25T08:10:00Z", 20.0),
            reading(&site1, &flow, "2026-04-25T08:20:00Z", 30.0),
        ],
    )
    .await;

    assert!(
        jobs_settled(&db, 60).await,
        "the write's follow-on jobs settle before the baseline"
    );
    refresh_views(&db, "2026-01-01", "2026-06-01").await;

    let near = ("2026-04-25T00:00:00Z", "2026-04-25T23:00:00Z");
    let far = ("2026-02-01T00:00:00Z", "2026-02-01T23:00:00Z");

    let near_before = aggregates(&app, &intern, &site1, "hourly", near.0, near.1).await;
    let far_before = aggregates(&app, &intern, &site1, "hourly", far.0, far.1).await;
    assert_populated(&near_before, "flagged-window baseline");
    assert_populated(&far_before, "distant-window baseline");
    assert_eq!(
        bucket(
            &near_before,
            &flow,
            "2026-04-25T08:00:00Z",
            "flagged-window baseline"
        ),
        Bucket {
            avg: Some(20.0),
            min: Some(10.0),
            max: Some(30.0),
            count: 3,
            flagged: 0
        },
        "the hour to be flagged starts at the mean of 10, 20 and 30: {near_before}"
    );
    let far_baseline = Bucket {
        avg: Some(150.0),
        min: Some(100.0),
        max: Some(200.0),
        count: 2,
        flagged: 0,
    };
    assert_eq!(
        bucket(
            &far_before,
            &flow,
            "2026-02-01T08:00:00Z",
            "distant-window baseline"
        ),
        far_baseline,
        "the distant hour starts at the mean of 100 and 200: {far_before}"
    );

    // Make the distant bucket genuinely stale, out of band, so "still 150.0" cannot be a tautology.
    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET raw_value = 400.0 \
             WHERE site_id = '{site1}' AND parameter_id = '{flow}' \
               AND time = '2026-02-01T08:30:00Z'::timestamptz"
        ),
    )
    .await;
    assert_eq!(
        live_hourly(&db, &site1, &flow, "2026-02-01T08:00:00Z").await,
        (Some(250.0), 2),
        "the distant hour's stored readings now average (100 + 400) / 2, so its rollup is stale"
    );

    let range = json!({
        "site_id": site1,
        "parameter_id": flow,
        "start_time": "2026-04-25T08:15:00Z",
        "end_time": "2026-04-25T08:25:00Z",
        "reason": "maintenance",
    });
    let (status, denied) =
        crate::common::patch_json_with_token(&app, "/api/readings/flag_range", &range, &intern)
            .await;
    assert_eq!(
        status, 403,
        "flagging is data curation, above the intern level: {denied}"
    );

    let (status, flagged) =
        crate::common::patch_json_with_token(&app, "/api/readings/flag_range", &range, &river)
            .await;
    assert_eq!(
        status, 200,
        "a river member flags the range ({status}): {flagged}"
    );
    assert!(
        flagged.contains("\"updated\":1"),
        "the narrow range selects exactly the third reading: {flagged}"
    );

    assert_eq!(
        flag_state(&db, &site1, &flow, "2026-04-25").await,
        vec![(10.0, false), (20.0, false), (30.0, true)],
        "flagging excludes a reading, it neither deletes it nor touches its siblings"
    );

    let near_flagged = aggregates(&app, &intern, &site1, "hourly", near.0, near.1).await;
    let far_flagged = aggregates(&app, &intern, &site1, "hourly", far.0, far.1).await;
    assert_populated(&near_flagged, "after flagging");
    assert_populated(&far_flagged, "distant window after flagging");
    assert_eq!(
        bucket(
            &near_flagged,
            &flow,
            "2026-04-25T08:00:00Z",
            "after flagging"
        ),
        Bucket {
            avg: Some(15.0),
            min: Some(10.0),
            max: Some(20.0),
            count: 2,
            flagged: 1
        },
        "the flagged reading leaves the rollup and shows in the flagged tally: {near_flagged}"
    );
    assert_eq!(
        bucket(
            &far_flagged,
            &flow,
            "2026-02-01T08:00:00Z",
            "distant window after flagging"
        ),
        far_baseline,
        "flagging refreshes [start - 32 days, end + 32 days] and nothing else, so an hour 83 days \
         away keeps the rollup it had, even though its stored readings now average 250.0: \
         {far_flagged}"
    );

    let (status, unflagged) = crate::common::patch_json_with_token(
        &app,
        "/api/readings/unflag_range",
        &json!({
            "site_id": site1,
            "parameter_id": flow,
            "start_time": "2026-04-25T08:15:00Z",
            "end_time": "2026-04-25T08:25:00Z",
        }),
        &river,
    )
    .await;
    assert_eq!(
        status, 200,
        "a river member unflags the range ({status}): {unflagged}"
    );
    assert!(
        unflagged.contains("\"updated\":1"),
        "the same one reading is unflagged: {unflagged}"
    );

    assert_eq!(
        flag_state(&db, &site1, &flow, "2026-04-25").await,
        vec![(10.0, false), (20.0, false), (30.0, false)],
        "unflagging restores the reading with its value intact"
    );

    let near_unflagged = aggregates(&app, &intern, &site1, "hourly", near.0, near.1).await;
    let far_unflagged = aggregates(&app, &intern, &site1, "hourly", far.0, far.1).await;
    assert_populated(&near_unflagged, "after unflagging");
    assert_populated(&far_unflagged, "distant window after unflagging");
    assert_eq!(
        bucket(
            &near_unflagged,
            &flow,
            "2026-04-25T08:00:00Z",
            "after unflagging"
        ),
        Bucket {
            avg: Some(20.0),
            min: Some(10.0),
            max: Some(30.0),
            count: 3,
            flagged: 0
        },
        "unflagging refreshes symmetrically, returning the reading to the mean: {near_unflagged}"
    );
    assert_eq!(
        bucket(
            &far_unflagged,
            &flow,
            "2026-02-01T08:00:00Z",
            "distant window after unflagging"
        ),
        far_baseline,
        "and still reaches no further than its own widened window: {far_unflagged}"
    );
}
