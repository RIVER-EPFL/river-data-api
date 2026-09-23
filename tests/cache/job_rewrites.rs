//! Jobs that rewrite stored readings in bulk (a reprocess, a slot merge, the janitor's curve-drift
//! recompose, a pairing plan's apply and revert) drop the cached series they changed.
//!
//! Each test primes a bounded read (so the freshness probe never runs and only write-side
//! invalidation can make the next read miss), runs the job through its route and the test worker,
//! then reads the same window again.
//!
//! Run: cargo test --test cache job_rewrites -- --test-threads=1

use axum::Router;
use axum::body::Body;
use http_body_util::BodyExt;
use serde_json::json;
use serial_test::serial;
use std::time::Duration;
use tower::ServiceExt;

use crate::common::sensor_lifecycle::*;
use crate::common::*;

const WINDOW: &str = "start=2025-01-10T00:00:00Z&end=2025-01-11T00:00:00Z";

struct Probe {
    cache: String,
    json: serde_json::Value,
    body: String,
}

async fn probe(app: &Router, uri: &str, token: &str) -> Probe {
    let request = axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status().as_u16(), 200, "GET {uri}");
    let cache = response
        .headers()
        .get("X-Cache")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("absent")
        .to_string();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&bytes).to_string();
    let json = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    Probe { cache, json, body }
}

/// The announcement reaches the cache through the bus subscriber's own task, so the read after a
/// job waits for it rather than assuming it already ran. Returns the first read that missed, or
/// the last hit.
async fn read_after_invalidation(app: &Router, uri: &str, token: &str) -> Probe {
    let mut last = probe(app, uri, token).await;
    for _ in 0..100 {
        if last.cache == "MISS" {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        last = probe(app, uri, token).await;
    }
    last
}

async fn prime(app: &Router, uri: &str, token: &str) -> Probe {
    let first = probe(app, uri, token).await;
    assert_eq!(first.cache, "MISS", "an empty cache cannot hit");
    assert_eq!(
        probe(app, uri, token).await.cache,
        "HIT",
        "the bounded read is cached"
    );
    first
}

#[tokio::test]
#[serial]
async fn a_calibration_reprocess_drops_the_sites_cached_readings() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Cache-probe", GLOBAL_PARAM_TEMP_ID).await;
    let dep = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    let stream = create_paired_stream(&db, "cache-reprocess", PARAM_S1_TEMP_ID).await;
    insert_readings(
        &db,
        stream,
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        sensor.id,
        sensor.base_calibration_id,
        dep,
        1.0,
        0.0,
        &[
            (dt("2025-01-10T10:00:00Z"), 10.0),
            (dt("2025-01-10T10:10:00Z"), 11.0),
        ],
    )
    .await;

    let (app, _state) = build_test_app_with_cache_and_state(db.clone());
    let token = seed_token_full(&db).await;
    let uri = format!("/api/sites/{SITE1_ID}/readings?{WINDOW}");

    let before = prime(&app, &uri, &token).await;
    assert_eq!(
        e2e::values_for(&before.json, GLOBAL_PARAM_TEMP_ID),
        vec![10.0, 11.0],
        "the 1:1 curve serves the raw values: {}",
        before.body
    );

    let (status, body) = put_json_with_token(
        &app,
        &format!("/api/sensor_calibrations/{}", sensor.base_calibration_id),
        &json!({ "slope": 2.0 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "calibration update: {body}");
    assert!(
        wait_for_reprocessing(&db, sensor.id, Duration::from_secs(10)).await,
        "the calibration reprocess runs to completion"
    );

    let after = read_after_invalidation(&app, &uri, &token).await;
    assert_eq!(
        after.cache, "MISS",
        "the reprocess rewrote the served values, so the cached entry must be gone: {}",
        after.body
    );
    assert_eq!(
        e2e::values_for(&after.json, GLOBAL_PARAM_TEMP_ID),
        vec![20.0, 22.0],
        "and the recomputed values are served: {}",
        after.body
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn a_slot_merge_drops_the_sites_cached_readings() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Cache-merge-probe", GLOBAL_PARAM_DO_ID).await;
    let dep = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    let stream = create_paired_stream(&db, "cache-merge", PARAM_S1_DO_ID).await;
    insert_readings(
        &db,
        stream,
        SITE1_ID,
        GLOBAL_PARAM_DO_ID,
        sensor.id,
        sensor.base_calibration_id,
        dep,
        1.0,
        0.0,
        &[(dt("2025-01-10T10:00:00Z"), 7.0)],
    )
    .await;

    let (app, _state) = build_test_app_with_cache_and_state(db.clone());
    let token = seed_token_full(&db).await;
    let uri = format!("/api/sites/{SITE1_ID}/readings?{WINDOW}");

    let before = prime(&app, &uri, &token).await;
    assert_eq!(
        e2e::values_for(&before.json, GLOBAL_PARAM_DO_ID),
        vec![7.0],
        "the reading is served under the source slot: {}",
        before.body
    );

    let (status, queued) = post_json_parse_with_token(
        &app,
        "/api/actions/merge_site_parameters",
        &json!({
            "source_site_parameter_id": PARAM_S1_DO_ID,
            "target_site_parameter_id": PARAM_S1_TEMP_ID,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "merge enqueued: {queued}");
    let job_id = queued["job_id"].as_str().expect("a job id").to_string();
    assert_eq!(
        jobs::wait_for_job(&db, &job_id).await,
        "completed",
        "the merge job completes"
    );

    let after = read_after_invalidation(&app, &uri, &token).await;
    assert_eq!(
        after.cache, "MISS",
        "the merge moved the site's readings to another parameter, so the cached entry must be \
         gone: {}",
        after.body
    );
    assert_eq!(
        e2e::values_for(&after.json, GLOBAL_PARAM_TEMP_ID),
        vec![7.0],
        "and the reading is served under the survivor: {}",
        after.body
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn a_janitor_curve_drift_recompose_drops_the_sites_cached_readings() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Cache-drift-probe", GLOBAL_PARAM_TEMP_ID).await;
    let dep = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    let stream = create_paired_stream(&db, "cache-drift", PARAM_S1_TEMP_ID).await;
    insert_readings(
        &db,
        stream,
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        sensor.id,
        sensor.base_calibration_id,
        dep,
        1.0,
        0.0,
        &[(dt("2025-01-10T10:00:00Z"), 10.0)],
    )
    .await;

    let (app, _state) = build_test_app_with_cache_and_state(db.clone());
    let token = seed_token_full(&db).await;
    let uri = format!("/api/sites/{SITE1_ID}/readings?{WINDOW}");

    let before = prime(&app, &uri, &token).await;
    assert_eq!(
        e2e::values_for(&before.json, GLOBAL_PARAM_TEMP_ID),
        vec![10.0],
        "the 1:1 curve serves the raw value: {}",
        before.body
    );

    // A statement runs no hook, so only the janitor's sweep can bring the stored value forward.
    exec(
        &db,
        &format!(
            "UPDATE sensor_calibrations SET slope = 3.0 WHERE id = '{}'",
            sensor.base_calibration_id
        ),
    )
    .await;
    let job_id = river_db::routes::private::reprocessing_jobs::service::enqueue(
        &db,
        "janitor_service",
        None,
        None,
        &json!({}),
        None,
    )
    .await
    .unwrap()
    .expect("a fresh janitor run is enqueued");
    assert_eq!(
        jobs::wait_for_job(&db, &job_id.to_string()).await,
        "completed",
        "the janitor run completes"
    );

    let after = read_after_invalidation(&app, &uri, &token).await;
    assert_eq!(
        after.cache, "MISS",
        "the sweep rewrote the served value, so the cached entry must be gone: {}",
        after.body
    );
    assert_eq!(
        e2e::values_for(&after.json, GLOBAL_PARAM_TEMP_ID),
        vec![30.0],
        "and the recomposed value is served: {}",
        after.body
    );

    cleanup_test_db(&db).await;
}

/// An unpaired stream holding one reading, and a draft plan pairing it to site 1's dissolved
/// oxygen, reviewed so the apply is allowed.
async fn draft_plan_over_history(
    db: &sea_orm::DatabaseConnection,
    app: &Router,
    token: &str,
) -> String {
    let stream_id = uuid::Uuid::new_v4();
    exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', 'vaisala', 'cache-plan-1', 'Cache Plan 1', true)"
        ),
    )
    .await;
    exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, time, raw_value, replicate_index) \
             VALUES ('{stream_id}', '2025-01-10T10:00:00Z', 5.0, 0)"
        ),
    )
    .await;
    let entries = json!([{
        "stream_id": stream_id,
        "source_key": "cache-plan-1",
        "source_name": "Cache Plan 1",
        "action": "pair",
        "project": { "id": PROJECT_ID, "name": "Test River Project", "create": false },
        "site": { "id": SITE1_ID, "name": "Upstream Station", "create": false, "latitude": null, "longitude": null, "altitude_m": null },
        "parameter": { "id": GLOBAL_PARAM_DO_ID, "name": "Dissolved Oxygen", "create": false, "units": "µM", "group_key": null, "original_names": [] },
        "confidence": "exact",
        "warnings": [],
        "original_parameter_name": null
    }]);
    let plan_id = uuid::Uuid::new_v4().to_string();
    exec(
        db,
        &format!(
            "INSERT INTO pairing_plans (id, source_system, status, summary, entries) \
             VALUES ('{plan_id}', 'vaisala', 'draft', '{{}}'::jsonb, '{}'::jsonb)",
            entries.to_string().replace('\'', "''")
        ),
    )
    .await;
    plans::acknowledge_plan(app, token, &plan_id).await;
    plan_id
}

async fn run_plan_action(
    db: &sea_orm::DatabaseConnection,
    app: &Router,
    token: &str,
    plan_id: &str,
    action: &str,
) {
    let (status, queued) = post_plan_action_parse_with_token(app, plan_id, action, token).await;
    assert!((200..300).contains(&status), "{action} enqueued: {queued}");
    let job_id = queued["job_id"].as_str().expect("a job id").to_string();
    assert_eq!(
        jobs::wait_for_job(db, &job_id).await,
        "completed",
        "the plan {action} job completes"
    );
}

#[tokio::test]
#[serial]
async fn a_plan_apply_and_its_revert_drop_the_sites_cached_readings() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let (app, _state) = build_test_app_with_cache_and_state(db.clone());
    let token = seed_token_full(&db).await;
    let plan_id = draft_plan_over_history(&db, &app, &token).await;
    let uri = format!("/api/sites/{SITE1_ID}/readings?{WINDOW}");

    let before = prime(&app, &uri, &token).await;
    assert!(
        e2e::values_for(&before.json, GLOBAL_PARAM_DO_ID).is_empty(),
        "an unpaired stream serves nothing at the site: {}",
        before.body
    );

    run_plan_action(&db, &app, &token, &plan_id, "apply").await;
    let applied = read_after_invalidation(&app, &uri, &token).await;
    assert_eq!(
        applied.cache, "MISS",
        "the apply attributed the stream's history to the site, so the cached entry must be \
         gone: {}",
        applied.body
    );
    assert_eq!(
        e2e::values_for(&applied.json, GLOBAL_PARAM_DO_ID),
        vec![5.0],
        "and the attributed reading is served: {}",
        applied.body
    );

    assert_eq!(
        probe(&app, &uri, &token).await.cache,
        "HIT",
        "the read is cached again"
    );
    run_plan_action(&db, &app, &token, &plan_id, "revert").await;
    let reverted = read_after_invalidation(&app, &uri, &token).await;
    assert_eq!(
        reverted.cache, "MISS",
        "the revert took the reading away from the site, so the cached entry must be gone: {}",
        reverted.body
    );
    assert!(
        e2e::values_for(&reverted.json, GLOBAL_PARAM_DO_ID).is_empty(),
        "and the site no longer serves it: {}",
        reverted.body
    );

    cleanup_test_db(&db).await;
}
