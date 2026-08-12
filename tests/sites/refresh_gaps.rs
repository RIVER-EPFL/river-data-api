//! Gaps between a change and the materialised state that is supposed to follow it: continuous
//! aggregates that never move, a refresh that fails without failing its job, and rows left behind
//! by a merge or a slot delete.
//!
//! Scenario: an operator lands data, then reshapes the catalog underneath it (merge two slots,
//! delete one, edit the janitor's cadence) or asks for a refresh.
//! Expected behaviour: every rollup the change invalidates is recomputed, every row the change
//! orphans travels with it, and a refresh that cannot run says so.
//!
//! Each test asserts bucket VALUES rather than a job status: `common/sync_state.rs` warn-logs and
//! drops every `refresh_continuous_aggregate` error, so "completed" is not evidence that anything
//! was materialised (that swallow is itself RD-041 below).
//!
//! Every fixture timestamp is in the past, since the refresh window is `[since, NOW()]`. Suites own
//! distinct months (2026-01 to 2026-05) so no two materialise the same bucket.
//!
//! These run as real Keycloak users and self-skip when Keycloak is unreachable, except RD-039,
//! which needs a worker whose registry carries the recurring Services (the shared builders register
//! only the on-demand jobs) and so builds its own app with an API token.
//!
//! Run: cargo test --test sites refresh_gaps -- --test-threads=1

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::{Value, json};
use serial_test::serial;

use crate::common::e2e;
use crate::common::keycloak as kc;
use crate::common::tracks;

// ============================================================================
// Harness
// ============================================================================

/// A Keycloak-authenticated app on a clean database, or `None` when Keycloak is unreachable.
async fn keycloak_app(test_name: &str) -> Option<(DatabaseConnection, Router, String)> {
    if !kc::require_keycloak_or_skip(test_name).await {
        return None;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    Some((db, app, admin))
}

fn instant(ts: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(ts)
        .unwrap_or_else(|e| panic!("fixture time {ts} is not RFC 3339: {e}"))
        .with_timezone(&Utc)
}

fn reading(site_id: &str, parameter_id: &str, time: &str, raw: f64) -> Value {
    json!({
        "site_id": site_id,
        "parameter_id": parameter_id,
        "time": time,
        "raw_value": raw,
        "measurement_type": "continuous",
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

/// The values a site readings response serves for one global parameter, empty when the series is
/// absent. Absence is a legitimate outcome here (a deleted slot drops out of the listing), so this
/// never panics on a missing series.
fn served_values(resp: &Value, parameter_id: &str) -> Vec<f64> {
    resp["parameters"]
        .as_array()
        .map(|ps| {
            ps.iter()
                .find(|p| p["parameter_id"] == parameter_id)
                .and_then(|p| p["values"].as_array())
                .map(|vs| vs.iter().filter_map(serde_json::Value::as_f64).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

async fn site_readings(app: &Router, jwt: &str, site_id: &str, start: &str, end: &str) -> Value {
    let uri = format!("/api/sites/{site_id}/readings?start={start}&end={end}");
    let (status, body) = crate::common::get_json_with_token(app, &uri, jwt).await;
    assert_eq!(status, 200, "GET {uri} ({status}): {body}");
    body
}

async fn daily_bucket(
    db: &DatabaseConnection,
    site_id: &str,
    parameter_id: &str,
    at: DateTime<Utc>,
) -> Option<(f64, i64)> {
    bucket_of(db, "readings_daily", "1 day", site_id, parameter_id, at).await
}

async fn weekly_bucket(
    db: &DatabaseConnection,
    site_id: &str,
    parameter_id: &str,
    at: DateTime<Utc>,
) -> Option<(f64, i64)> {
    bucket_of(db, "readings_weekly", "1 week", site_id, parameter_id, at).await
}

/// A rollup bucket outside the hourly view, collapsed over the sensor dimension the way
/// `sites/aggregates.rs` does (`SUM(sum_value) / SUM(count)`, not a mean of per-sensor means).
/// The hourly view has [`e2e::hourly_bucket`]; this covers daily and weekly.
async fn bucket_of(
    db: &DatabaseConnection,
    view: &str,
    width: &str,
    site_id: &str,
    parameter_id: &str,
    at: DateTime<Utc>,
) -> Option<(f64, i64)> {
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT SUM(sum_value) AS total, SUM(count)::bigint AS n FROM {view} \
                 WHERE site_id = '{site_id}' AND parameter_id = '{parameter_id}' \
                   AND bucket = time_bucket('{width}', '{}'::timestamptz)",
                at.to_rfc3339()
            ),
        ))
        .await
        .unwrap_or_else(|e| panic!("query {view}: {e}"))?;
    let total: Option<f64> = row.try_get("", "total").ok().flatten();
    let n: Option<i64> = row.try_get("", "n").ok().flatten();
    match (total, n) {
        (Some(t), Some(c)) if c > 0 => Some((t / c as f64, c)),
        _ => None,
    }
}

/// A tracked job as the API reports it, polled until it settles. Settled means terminal or, for a
/// job that failed and is waiting on a backoff, carrying an error. Parses leniently so a caller
/// that must run teardown before asserting is never taken out by a panic mid-poll.
async fn poll_job_view(app: &Router, jwt: &str, job_id: &str, max_secs: u64) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(max_secs);
    loop {
        let (_status, body) = crate::common::get_with_token(
            app,
            &format!("/api/reprocessing_jobs/{job_id}"),
            jwt,
        )
        .await;
        let job: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let status = job["status"].as_str().unwrap_or("").to_string();
        let errored = job["error_message"].as_str().is_some_and(|m| !m.is_empty());
        if matches!(status.as_str(), "completed" | "failed" | "cancelled")
            || errored
            || std::time::Instant::now() >= deadline
        {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// POST an action that returns `{job_id}`, then wait for that job and return its final view.
async fn run_action(app: &Router, jwt: &str, path: &str, body: &Value) -> Value {
    let (status, queued) = crate::common::post_json_parse_with_token(app, path, body, jwt).await;
    assert!(
        (200..300).contains(&status),
        "POST {path} ({status}): {queued}"
    );
    let job_id = queued["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("POST {path} returns a job_id: {queued}"))
        .to_string();
    poll_job_view(app, jwt, &job_id, 60).await
}

/// The operator-facing full refresh, waited to a terminal state.
async fn refresh_all(app: &Router, jwt: &str) -> Value {
    run_action(
        app,
        jwt,
        "/api/actions/refresh_aggregates",
        &json!({ "full": true }),
    )
    .await
}

async fn jobs_settled(db: &DatabaseConnection, timeout_secs: u64) -> bool {
    let start = std::time::Instant::now();
    loop {
        let active = e2e::count(
            db,
            "SELECT COUNT(*)::bigint FROM reprocessing_jobs \
             WHERE status IN ('queued', 'pending', 'running', 'retrying')",
        )
        .await;
        if active == 0 {
            return true;
        }
        if start.elapsed().as_secs() > timeout_secs {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn filter_query(field: &str, value: &str) -> String {
    e2e::percent_encode(&format!(r#"{{"{field}":"{value}"}}"#))
}

async fn list_len(app: &Router, jwt: &str, path: &str, field: &str, value: &str) -> usize {
    let uri = format!("/api/{path}?filter={}", filter_query(field, value));
    let (status, body) = crate::common::get_json_with_token(app, &uri, jwt).await;
    assert_eq!(status, 200, "GET {uri} ({status}): {body}");
    body.as_array()
        .unwrap_or_else(|| panic!("GET {uri} returns an array: {body}"))
        .len()
}

// ============================================================================
// RD-039
// ============================================================================

/// RD-039: with the janitor on a 6-hourly operator cadence, the full aggregate refresh must fire on
/// exactly one of the day's four slots, not on none of them (or all four).
#[tokio::test]
#[serial]
async fn janitor_full_refresh_follows_the_operator_cadence() {
    use river_db::common::AppState;
    use river_db::routes::private::reprocessing_jobs::{job, scheduler, worker};

    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    // The shared builders spawn a worker over `build_registry()` alone, which has no handler for
    // `janitor_service`: a job enqueued for it would be failed as unregistered. This app carries
    // the recurring Services, exactly as `main.rs` assembles them.
    let config = river_db::config::Config {
        cache_ttl_seconds: 0,
        cache_max_bytes: 0,
        ..crate::common::cached_test_config()
    };
    let mut registry = job::build_registry();
    job::register_scheduled_services(&mut registry, &config);
    let registry = Arc::new(registry);
    let state = AppState::new(db.clone(), config, None);
    let app = river_db::routes::build_router(state.clone());
    tokio::spawn({
        let db = db.clone();
        let events = state.events.clone();
        let registry = registry.clone();
        async move {
            worker::run(db, events, registry, std::future::pending::<()>()).await;
        }
    });
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;

    scheduler::seed_default_schedules(&db, &registry)
        .await
        .expect("seed the recurring-service schedules");

    let (status, schedules) =
        crate::common::get_json_with_token(&app, "/api/schedules", &token).await;
    assert_eq!(status, 200, "GET /api/schedules ({status}): {schedules}");
    let names: Vec<String> = schedules
        .as_array()
        .unwrap_or_else(|| panic!("schedules list is an array: {schedules}"))
        .iter()
        .filter_map(|s| s["job_name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.iter().any(|n| n == "janitor_service"),
        "the janitor seeds a schedule row: {schedules}"
    );
    for name in names.iter().filter(|n| *n != "janitor_service") {
        let (status, body) = crate::common::patch_json_with_token(
            &app,
            &format!("/api/schedules/{name}"),
            &json!({ "enabled": false }),
            &token,
        )
        .await;
        assert_eq!(status, 200, "disable schedule {name} ({status}): {body}");
    }

    let (status, patched) = crate::common::patch_json_with_token(
        &app,
        "/api/schedules/janitor_service",
        &json!({ "interval_seconds": 21_600 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "set the janitor to a 6-hourly cadence ({status}): {patched}");
    let view: Value = serde_json::from_str(&patched).unwrap_or(Value::Null);
    assert_eq!(view["interval_seconds"], 21_600, "the cadence is stored: {patched}");

    let track = tracks::onboard_csv_track(&app, &token).await;
    let parameter_id = track.parameter_id("TrkCsvDepth").to_string();
    assert!(jobs_settled(&db, 60).await, "provisioning settles before the fixtures land");

    // One 6-hourly slot per iteration, over a past UTC day: 00:00 opens a full-refresh period, the
    // other three do not. Each slot gets its own reading, seven months old so no incremental
    // refresh (hourly window `NOW() - 24h`) can reach it: a materialised bucket therefore means a
    // full refresh ran on that slot and nothing else.
    let slots = [
        "2026-08-01T00:00:00Z",
        "2026-08-01T06:00:00Z",
        "2026-08-01T12:00:00Z",
        "2026-08-01T18:00:00Z",
    ];
    let mut refreshed_on: Vec<&str> = Vec::new();
    for (i, slot) in slots.into_iter().enumerate() {
        let at = format!("2026-01-15T{i:02}:00:00Z");
        let value = 110.0 + i as f64;
        let row = reading(&track.site_id, &parameter_id, &at, value);
        write_readings(&app, &token, vec![row]).await;
        assert!(jobs_settled(&db, 60).await, "the batch's follow-on jobs settle before the tick");
        assert!(
            e2e::hourly_bucket(&db, &track.site_id, &parameter_id, instant(&at))
                .await
                .is_none(),
            "the {at} bucket is unmaterialised before the {slot} janitor run"
        );

        // No endpoint places a schedule on a chosen slot (PATCH always recomputes `next_run_at` off
        // now), and the slot is precisely the input under test.
        crate::common::exec(
            &db,
            &format!(
                "UPDATE schedules SET next_run_at = '{slot}' \
                 WHERE job_name = 'janitor_service'"
            ),
        )
        .await;
        let enqueued = scheduler::tick(&db, &registry)
            .await
            .expect("scheduler tick");
        assert_eq!(enqueued, 1, "the due janitor slot {slot} enqueues exactly one run");
        assert!(
            e2e::wait_for_jobs_by_trigger(&db, "janitor_service", 90).await,
            "the janitor run for slot {slot} completes"
        );

        let bucket = e2e::hourly_bucket(&db, &track.site_id, &parameter_id, instant(&at)).await;
        if let Some((mean, count)) = bucket {
            assert!(
                (mean - value).abs() < 1e-9 && count == 1,
                "slot {slot} materialised {at} as ({mean}, {count}), expected ({value}, 1)"
            );
            refreshed_on.push(slot);
        }
    }

    assert_eq!(
        refreshed_on.len(),
        1,
        "a 6-hourly janitor must run its full aggregate refresh once per 24h period, on the slot \
         that opens it; slots that ran it: {refreshed_on:?}"
    );

    crate::common::cleanup_test_db(&db).await;
}

// ============================================================================
// RD-040
// ============================================================================

/// RD-040: merging two site parameters moves the readings, so the survivor's rollups must move with
/// them and the absorbed parameter's must go.
#[tokio::test]
#[serial]
async fn merging_site_parameters_moves_the_rollups_with_the_readings() {
    let Some((db, app, admin)) = keycloak_app("merge_moves_rollups").await else {
        return;
    };

    let track = tracks::onboard_csv_track(&app, &admin).await;
    let source = track.parameter_id("TrkCsvDepth").to_string();
    let target = track.parameter_id("TrkCsvTurb").to_string();
    let source_slot = track.site_parameter_ids[0].clone();
    let target_slot = track.site_parameter_ids[1].clone();

    let day = "2026-02-10";
    write_readings(
        &app,
        &admin,
        vec![
            reading(&track.site_id, &source, &format!("{day}T10:00:00Z"), 110.0),
            reading(&track.site_id, &source, &format!("{day}T10:30:00Z"), 130.0),
        ],
    )
    .await;
    assert!(jobs_settled(&db, 60).await, "ingestion settles before the rollup is built");

    // Bucket-aligned start, so the fixture is materialised whatever TimescaleDB does with a partial
    // leading bucket (that alignment question is RD-044's, not this test's).
    e2e::refresh_hourly(&db, instant(&format!("{day}T00:00:00Z"))).await;
    let at = instant(&format!("{day}T10:00:00Z"));
    assert_eq!(
        e2e::hourly_bucket(&db, &track.site_id, &source, at).await,
        Some((120.0, 2)),
        "the source slot's hourly bucket holds the mean of 110 and 130 before the merge"
    );
    assert_eq!(
        e2e::hourly_bucket(&db, &track.site_id, &target, at).await,
        None,
        "the target slot has no data of its own before the merge"
    );

    let job = run_action(
        &app,
        &admin,
        "/api/actions/merge_site_parameters",
        &json!({
            "source_site_parameter_id": source_slot,
            "target_site_parameter_id": target_slot,
        }),
    )
    .await;
    assert_eq!(job["status"], "completed", "the merge job completes: {job}");

    let served = site_readings(
        &app,
        &admin,
        &track.site_id,
        &format!("{day}T00:00:00Z"),
        &format!("{day}T23:59:59Z"),
    )
    .await;
    assert_eq!(
        served_values(&served, &target),
        vec![110.0, 130.0],
        "the readings themselves are served under the survivor: {served}"
    );
    assert!(
        served_values(&served, &source).is_empty(),
        "and no longer under the absorbed parameter: {served}"
    );

    assert_eq!(
        e2e::hourly_bucket(&db, &track.site_id, &target, at).await,
        Some((120.0, 2)),
        "the survivor's rollup must carry the moved readings, or the chart shows an empty series \
         while /readings shows the data"
    );
    assert_eq!(
        e2e::hourly_bucket(&db, &track.site_id, &source, at).await,
        None,
        "and the absorbed parameter's rollup must be gone, not left standing on deleted readings"
    );

    crate::common::cleanup_test_db(&db).await;
}

// ============================================================================
// RD-041
// ============================================================================

/// RD-041: a refresh whose `refresh_continuous_aggregate` call errors must fail its job, not report
/// completed with no error.
#[tokio::test]
#[serial]
async fn a_refresh_that_cannot_run_fails_its_job() {
    let Some((db, app, admin)) = keycloak_app("failed_refresh_fails_job").await else {
        return;
    };

    let track = tracks::onboard_csv_track(&app, &admin).await;
    let parameter_id = track.parameter_id("TrkCsvDepth").to_string();
    let day = "2026-03-12";
    write_readings(
        &app,
        &admin,
        vec![reading(&track.site_id, &parameter_id, &format!("{day}T08:00:00Z"), 140.0)],
    )
    .await;
    assert!(jobs_settled(&db, 60).await, "ingestion settles before the refresh is asked for");

    // Renaming the view is the one deterministic way to make the CALL error. Everything between
    // here and the rename back is kept assertion-free so the view is always restored.
    crate::common::exec(
        &db,
        "ALTER MATERIALIZED VIEW readings_hourly RENAME TO readings_hourly_renamed",
    )
    .await;
    let broken = refresh_all(&app, &admin).await;
    crate::common::exec(
        &db,
        "ALTER MATERIALIZED VIEW readings_hourly_renamed RENAME TO readings_hourly",
    )
    .await;

    assert_ne!(
        broken["status"], "completed",
        "a refresh that could not touch readings_hourly must not report success; a failed refresh \
         is failed (and retried), not completed: {broken}"
    );
    assert!(
        broken["error_message"].as_str().is_some_and(|m| !m.is_empty()),
        "and it must carry the database's error, so an operator can see what went wrong: {broken}"
    );

    let healthy = refresh_all(&app, &admin).await;
    assert_eq!(
        healthy["status"], "completed",
        "with the view back, the same request completes, so the failure above was the view and \
         not the endpoint: {healthy}"
    );
    let at = instant(&format!("{day}T08:00:00Z"));
    assert_eq!(
        e2e::hourly_bucket(&db, &track.site_id, &parameter_id, at).await,
        Some((140.0, 1)),
        "and that completed refresh really materialised the bucket"
    );

    crate::common::cleanup_test_db(&db).await;
}

// ============================================================================
// RD-042
// ============================================================================

/// RD-042: a site-parameter merge must carry the source's grab samples and annotations to the
/// survivor, not strand them on the deleted slot.
#[tokio::test]
#[serial]
async fn merging_site_parameters_carries_samples_and_annotations() {
    let Some((db, app, admin)) = keycloak_app("merge_carries_samples").await else {
        return;
    };

    let track = tracks::onboard_grab_track(&app, &admin).await;
    let source = track.parameter_id("TrkGrabDoc").to_string();
    let source_slot = track.site_parameter_ids[0].clone();
    let target =
        e2e::create_parameter(&app, &admin, "TrkGrabDocAlt", "Track Grab DOC alias", "ppb").await;
    let target_slot =
        e2e::assign_site_parameter_minimal(&app, &admin, &track.site_id, &target).await;

    let day = "2026-04-14";
    let grab_at = format!("{day}T09:00:00Z");
    let (status, grab) = crate::common::post_json_parse_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": track.site_id,
            "label": "merge fixture",
            "readings": tracks::grab_replicates(&source, &grab_at, &[310.0, 330.0]),
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "grab entry ({status}): {grab}");
    assert_eq!(grab["samples_created"], 1, "the replicate pair is one sample: {grab}");

    // Inside the export's default [T + 2h, T + 6h] window, mean 310, and inside the grab track's
    // value band so a reading from elsewhere could not pass for one of these.
    write_readings(
        &app,
        &admin,
        vec![
            reading(&track.site_id, &source, &format!("{day}T11:00:00Z"), 300.0),
            reading(&track.site_id, &source, &format!("{day}T13:00:00Z"), 320.0),
        ],
    )
    .await;

    let (status, annotation) = crate::common::post_json_parse_with_token(
        &app,
        "/api/annotations",
        &json!({
            "site_id": track.site_id,
            "parameter_id": source,
            "start_time": format!("{day}T00:00:00Z"),
            "end_time": format!("{day}T23:59:59Z"),
            "text": "bottle handled warm",
            "category": "quality",
        }),
        &admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "annotate the slot ({status}): {annotation}"
    );
    assert!(jobs_settled(&db, 60).await, "the fixtures settle before the merge");

    let export_uri = |parameter_id: &str| {
        format!(
            "/api/sites/{}/export/sensor-vs-grab?parameter_id={parameter_id}\
             &start={day}T00:00:00Z&end={day}T23:59:59Z",
            track.site_id
        )
    };
    let (status, before) =
        crate::common::get_json_with_token(&app, &export_uri(&source), &admin).await;
    assert_eq!(status, 200, "comparison export before the merge ({status}): {before}");
    let rows = before["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 1, "the source slot pairs its one grab: {before}");
    assert!(
        (rows[0]["grab_value"].as_f64().unwrap_or(f64::NAN) - 320.0).abs() < 1e-9,
        "the grab value is the replicate mean: {before}"
    );
    assert!(
        (rows[0]["sensor_avg"].as_f64().unwrap_or(f64::NAN) - 310.0).abs() < 1e-9,
        "and the sensor side averages the two post-grab readings: {before}"
    );

    let job = run_action(
        &app,
        &admin,
        "/api/actions/merge_site_parameters",
        &json!({
            "source_site_parameter_id": source_slot,
            "target_site_parameter_id": target_slot,
        }),
    )
    .await;
    assert_eq!(job["status"], "completed", "the merge job completes: {job}");

    let (status, after) =
        crate::common::get_json_with_token(&app, &export_uri(&target), &admin).await;
    assert_eq!(status, 200, "comparison export after the merge ({status}): {after}");
    let rows = after["rows"].as_array().expect("rows array");
    assert_eq!(
        rows.len(),
        1,
        "the survivor inherits the grab, or the merge silently empties the grab-comparison \
         export: {after}"
    );
    assert!(
        (rows[0]["grab_value"].as_f64().unwrap_or(f64::NAN) - 320.0).abs() < 1e-9,
        "with the same grab value: {after}"
    );
    assert!(
        (rows[0]["sensor_avg"].as_f64().unwrap_or(f64::NAN) - 310.0).abs() < 1e-9,
        "paired against the same continuous window, which moved with the readings: {after}"
    );

    assert_eq!(
        list_len(&app, &admin, "samples", "parameter_id", &target).await,
        1,
        "the sample row itself is re-pointed at the survivor"
    );
    assert_eq!(
        list_len(&app, &admin, "samples", "parameter_id", &source).await,
        0,
        "and none is left on the deleted slot, unreachable from any live parameter"
    );
    assert_eq!(
        list_len(&app, &admin, "annotations", "parameter_id", &target).await,
        1,
        "annotations are keyed (site, parameter) too, so they travel with the merge"
    );
    assert_eq!(
        list_len(&app, &admin, "annotations", "parameter_id", &source).await,
        0,
        "and none is stranded on the absorbed parameter"
    );

    crate::common::cleanup_test_db(&db).await;
}

// ============================================================================
// RD-043
// ============================================================================

/// RD-043: deleting a site parameter must either be refused while a stream is paired to it or tear
/// the slot's readings down, never leave half of the stream's data attributed and half not.
#[tokio::test]
#[serial]
async fn deleting_a_slot_does_not_split_its_streams_attribution() {
    let Some((db, app, admin)) = keycloak_app("delete_slot_attribution").await else {
        return;
    };

    let track = tracks::onboard_sensor_flow_track(&app, &admin).await;
    let parameter_id = track.parameter_id("TrkFlowDO").to_string();
    let stream_id = track.stream_ids[0].clone();
    let slot = track.site_parameter_ids[0].clone();

    let (status, paired) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({ "site_parameter_id": slot }),
        &admin,
    )
    .await;
    assert!((200..300).contains(&status), "pair the stream ({status}): {paired}");

    let ingest = |cycle: usize| {
        let app = app.clone();
        let admin = admin.clone();
        let stream_id = stream_id.clone();
        async move {
            let (status, body) = crate::common::post_json_parse_with_token(
                &app,
                "/api/ingest",
                &json!({ "stream_id": stream_id, "readings": tracks::flow_cycle_readings(cycle) }),
                &admin,
            )
            .await;
            assert_eq!(status, 200, "ingest cycle {cycle} ({status}): {body}");
        }
    };

    ingest(0).await;
    assert!(jobs_settled(&db, 60).await, "the first cycle's follow-on jobs settle");
    let window = (
        format!("{}T00:00:00Z", tracks::FLOW_BASE_DAY),
        format!("{}T01:00:00Z", tracks::FLOW_BASE_DAY),
    );
    let before = site_readings(&app, &admin, &track.site_id, &window.0, &window.1).await;
    assert_eq!(
        served_values(&before, &parameter_id).len(),
        tracks::FLOW_READINGS_PER_CYCLE,
        "the paired cycle is served under the slot: {before}"
    );

    let (delete_status, delete_body) = crate::common::delete_with_token(
        &app,
        &format!("/api/site_parameters/{slot}"),
        &admin,
    )
    .await;

    ingest(1).await;
    assert!(jobs_settled(&db, 60).await, "the second cycle's follow-on jobs settle");

    let total = e2e::count(
        &db,
        &format!("SELECT COUNT(*)::bigint FROM readings WHERE stream_id = '{stream_id}'"),
    )
    .await;
    assert_eq!(
        total,
        (2 * tracks::FLOW_READINGS_PER_CYCLE) as i64,
        "both cycles are stored whatever the delete did"
    );

    let attributed = e2e::count(
        &db,
        &format!(
            "SELECT COUNT(*)::bigint FROM readings \
             WHERE stream_id = '{stream_id}' AND site_id IS NOT NULL AND parameter_id IS NOT NULL"
        ),
    )
    .await;
    let stream_uri = format!("/api/data_streams/{stream_id}");
    let (status, stream) = crate::common::get_json_with_token(&app, &stream_uri, &admin).await;
    assert_eq!(status, 200, "read the stream back ({status}): {stream}");
    let still_paired = !stream["site_parameter_id"].is_null();

    let after = site_readings(&app, &admin, &track.site_id, &window.0, &window.1).await;
    let served = served_values(&after, &parameter_id).len();

    let refused = (400..600).contains(&delete_status)
        && attributed == total
        && still_paired
        && served == total as usize;
    let torn_down = (200..300).contains(&delete_status)
        && attributed == 0
        && !still_paired
        && served == 0;
    assert!(
        refused || torn_down,
        "deleting a slot with a paired stream must either be refused (leaving all {total} readings \
         attributed and the stream paired) or perform the unpair teardown (leaving none attributed). \
         Delete returned {delete_status}: {delete_body}. Attributed: {attributed} of {total}, \
         still paired: {still_paired}, served: {served}"
    );

    crate::common::cleanup_test_db(&db).await;
}

// ============================================================================
// RD-044
// ============================================================================

/// RD-044: an incremental refresh started at a reading's timestamp must materialise the hourly and
/// daily buckets containing it, not only the ones starting after it.
#[tokio::test]
#[serial]
async fn an_incremental_refresh_covers_the_bucket_containing_its_start() {
    let Some((db, app, admin)) = keycloak_app("refresh_covers_leading_bucket").await else {
        return;
    };

    let track = tracks::onboard_csv_track(&app, &admin).await;
    let parameter_id = track.parameter_id("TrkCsvDepth").to_string();

    // Two rows on one past day, neither on a bucket boundary. The import job refreshes from the
    // earliest timestamp verbatim, so 14:22 is what the hourly and daily windows start at.
    let day = "2026-05-06";
    let csv = format!(
        "DateTime,TrkCsvDepth\n{day}T14:22:00Z,110.00\n{day}T16:10:00Z,130.00\n"
    );
    let (status, imported) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &json!({ "site": track.site_id, "csv": csv, "dry_run": false }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "CSV import ({status}): {imported}");
    assert_eq!(imported["row_count"], 2, "both rows parse: {imported}");
    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "csv_import", 60).await,
        "the csv_import job runs and succeeds, and it is that job which refreshes the aggregates"
    );
    assert!(jobs_settled(&db, 60).await, "its follow-on jobs settle too");

    let first = instant(&format!("{day}T14:22:00Z"));
    let second = instant(&format!("{day}T16:10:00Z"));
    let site = track.site_id.clone();
    let hourly_first = e2e::hourly_bucket(&db, &site, &parameter_id, first).await;
    let hourly_second = e2e::hourly_bucket(&db, &site, &parameter_id, second).await;
    let daily = daily_bucket(&db, &site, &parameter_id, first).await;
    let weekly = weekly_bucket(&db, &site, &parameter_id, first).await;

    let full = refresh_all(&app, &admin).await;
    assert_eq!(full["status"], "completed", "the control full refresh completes: {full}");
    let hourly_first_full = e2e::hourly_bucket(&db, &site, &parameter_id, first).await;
    let daily_full = daily_bucket(&db, &site, &parameter_id, first).await;

    assert_eq!(
        hourly_first_full,
        Some((110.0, 1)),
        "control: the 14:00 hourly bucket does materialise when the window covers it"
    );
    assert_eq!(
        daily_full,
        Some((120.0, 2)),
        "control: so does the day, holding the mean of 110 and 130"
    );
    assert_eq!(
        weekly,
        Some((120.0, 2)),
        "regression guard: the weekly view, whose start is widened by 7 days, already held the \
         week before any manual refresh"
    );
    assert_eq!(
        hourly_second,
        Some((130.0, 1)),
        "the 16:00 bucket, which starts after the refresh window's start, was materialised by the \
         import's own refresh"
    );
    assert_eq!(
        hourly_first,
        Some((110.0, 1)),
        "and so must the 14:00 bucket be: a refresh started at 14:22 must not drop the bucket the \
         imported reading is in"
    );
    assert_eq!(
        daily,
        Some((120.0, 2)),
        "same for the day: a window starting mid-day must still refresh that day's bucket"
    );

    crate::common::cleanup_test_db(&db).await;
}
