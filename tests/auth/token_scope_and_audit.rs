//! End-to-end access-control workflow for external API keys.
//!
//! Seeds two projects from scratch, issues a key, and exercises the full matrix the team asked for:
//! a request WITH the right key, WITH a wrong key (wrong permission / wrong project), and WITHOUT a
//! key; cross-project confinement on every write path; cross-project READ confinement on the
//! auxiliary read endpoints (search, stream stats, sensor series/bands, calibration window); and the
//! forensic audit log. Token *creation* is admin-only (Keycloak), so an admin-issued key is seeded
//! directly, the same representation used by `e2e_api_token_lifecycle_test`.

use axum::extract::{Path, State};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;
use std::time::Duration;

use crate::common::fixtures::{GLOBAL_PARAM_DEPTH_ID, GLOBAL_PARAM_TEMP_ID, PROJECT_ID, SITE1_ID};
use crate::common::sensor_lifecycle as slc;

const PROJECT_B_ID: &str = "00000000-0000-4000-b000-000000000001";
const SITE_B_ID: &str = "00000000-0000-4000-b000-000000000010";
const SP_B_DEPTH_ID: &str = "00000000-0000-4000-b000-000000000020";
/// Project-A seeded depth stream (paired to PARAM_S1_DEPTH at SITE1), see `seed_streams_for_site_params`.
const A_DEPTH_STREAM_ID: &str = "00000000-0000-4000-d000-000000000005";

fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

async fn token_id(db: &DatabaseConnection, raw: &str) -> uuid::Uuid {
    let prefix = raw
        .strip_prefix("rvd_")
        .and_then(|r| r.split_once('_'))
        .map(|(p, _)| p)
        .expect("api token must be rvd_<prefix>_<secret>");
    let row = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT id FROM api_tokens WHERE token_prefix = $1",
            [prefix.into()],
        ))
        .await
        .unwrap()
        .expect("token row");
    row.try_get::<uuid::Uuid>("", "id").unwrap()
}

/// Project A (full seed) + Project B with a site named "Station B" (so it competes in `search`),
/// plus a Project-B depth site_parameter for cross-project ingest.
async fn setup() -> (DatabaseConnection, axum::Router, river_db::common::AppState) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO projects (id, name, description) \
             VALUES ('{PROJECT_B_ID}', 'Project B', 'second project')"
        ),
    )
    .await;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO sites (id, name, project_id) \
             VALUES ('{SITE_B_ID}', 'Station B', '{PROJECT_B_ID}')"
        ),
    )
    .await;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO site_parameters \
             (id, site_id, parameter_id, name, sensor_type, display_units, units_name, \
              units_min, units_max, decimal_places, sample_interval_sec, is_active) \
             VALUES ('{SP_B_DEPTH_ID}', '{SITE_B_ID}', '{GLOBAL_PARAM_DEPTH_ID}', 'Depth', 'Depth', \
                     'mm', 'Millimeters', 0, 3000, 0, 600, true)"
        ),
    )
    .await;
    let (app, state) = crate::common::build_test_app_with_state(db.clone());
    (db, app, state)
}

#[tokio::test]
#[serial]
async fn auth_matrix_with_wrong_and_without_key() {
    let (db, app, _state) = setup().await;
    let t = now_rfc3339();
    let body = serde_json::json!({
        "readings": [{ "site_id": SITE1_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": t, "raw_value": 1.0 }]
    });

    // WITH the right key (scoped to Project A, write_data): success.
    let right = crate::common::seed_api_token(
        &db,
        crate::common::perms(true, true, false, true),
        Some(PROJECT_ID),
    )
    .await;
    let (s, b) =
        crate::common::post_json_with_token(&app, "/api/readings/batch", &body, &right).await;
    assert_eq!(s, 200, "correct key must succeed: {b}");

    // WITHOUT a key: 401 (no Authorization header on a read, and an empty bearer on a write).
    let (s, _) = crate::common::get(&app, "/api/sites").await;
    assert_eq!(s, 401, "no key must be unauthorized");
    let (s, _) = crate::common::post_json_with_token(&app, "/api/readings/batch", &body, "").await;
    assert_eq!(s, 401, "empty bearer must be unauthorized");

    // WRONG key #1, valid key but lacking the permission (read-only): 403 on a write.
    let read_only = crate::common::seed_api_token(
        &db,
        crate::common::perms(true, true, false, false),
        Some(PROJECT_ID),
    )
    .await;
    let (s, _) =
        crate::common::post_json_with_token(&app, "/api/readings/batch", &body, &read_only).await;
    assert_eq!(
        s, 403,
        "a key without write_data must be forbidden on a write"
    );

    // WRONG key #2, valid write key but scoped to the OTHER project: 403 cross-project.
    let other_project = crate::common::seed_api_token(
        &db,
        crate::common::perms(true, true, false, true),
        Some(PROJECT_B_ID),
    )
    .await;
    let (s, _) =
        crate::common::post_json_with_token(&app, "/api/readings/batch", &body, &other_project)
            .await;
    assert_eq!(
        s, 403,
        "a key scoped to another project must not write here"
    );
}

#[tokio::test]
#[serial]
async fn write_paths_confined_ingest_status_and_flags() {
    let (db, app, _state) = setup().await;
    let key = crate::common::seed_api_token(
        &db,
        crate::common::perms(true, true, false, true),
        Some(PROJECT_ID),
    )
    .await;
    let t = now_rfc3339();

    // Stream-based ingest: in-scope (Project-A depth stream) succeeds...
    let ingest_in = serde_json::json!({
        "stream_id": A_DEPTH_STREAM_ID,
        "readings": [{ "time": t, "raw_value": 1.0 }]
    });
    let (s, b) = crate::common::post_json_with_token(&app, "/api/ingest", &ingest_in, &key).await;
    assert_eq!(s, 200, "in-scope ingest must succeed: {b}");

    // ...cross-project (a Project-B paired stream) is forbidden.
    let b_stream = slc::create_paired_stream(&db, "key-test-b-ingest", SP_B_DEPTH_ID).await;
    let ingest_out = serde_json::json!({
        "stream_id": b_stream.to_string(),
        "readings": [{ "time": t, "raw_value": 1.0 }]
    });
    let (s, _) = crate::common::post_json_with_token(&app, "/api/ingest", &ingest_out, &key).await;
    assert_eq!(s, 403, "cross-project ingest must be forbidden");

    // Stream status-event ingest: cross-project forbidden.
    let status_out = serde_json::json!({
        "stream_id": b_stream.to_string(),
        "events": [{ "time": t, "value": "low_battery" }]
    });
    let (s, _) =
        crate::common::post_json_with_token(&app, "/api/ingest/status_events", &status_out, &key)
            .await;
    assert_eq!(
        s, 403,
        "cross-project status-event ingest must be forbidden"
    );

    // Flag/unflag (point + range): in-scope allowed, cross-project forbidden. (The scope check runs
    // before the UPDATE, so it holds even though no reading matches the synthetic timestamp.)
    let flag_in = serde_json::json!({
        "readings": [{ "site_id": SITE1_ID, "parameter_id": GLOBAL_PARAM_DEPTH_ID, "time": t }],
        "reason": "qa"
    });
    let (s, b) =
        crate::common::patch_json_with_token(&app, "/api/readings/flag", &flag_in, &key).await;
    assert_eq!(s, 200, "in-scope flag must succeed: {b}");

    let flag_out = serde_json::json!({
        "readings": [{ "site_id": SITE_B_ID, "parameter_id": GLOBAL_PARAM_DEPTH_ID, "time": t }],
        "reason": "qa"
    });
    let (s, _) =
        crate::common::patch_json_with_token(&app, "/api/readings/flag", &flag_out, &key).await;
    assert_eq!(s, 403, "cross-project flag must be forbidden");

    let range_out = serde_json::json!({
        "site_id": SITE_B_ID, "parameter_id": GLOBAL_PARAM_DEPTH_ID,
        "start_time": t, "end_time": t, "reason": "qa"
    });
    let (s, _) =
        crate::common::patch_json_with_token(&app, "/api/readings/flag_range", &range_out, &key)
            .await;
    assert_eq!(s, 403, "cross-project flag_range must be forbidden");
    let (s, _) =
        crate::common::patch_json_with_token(&app, "/api/readings/unflag_range", &range_out, &key)
            .await;
    assert_eq!(s, 403, "cross-project unflag_range must be forbidden");
}

#[tokio::test]
#[serial]
async fn read_endpoints_filtered_to_token_project() {
    let (db, app, _state) = setup().await;

    // A sensor deployed in Project A and another in Project B, each with an identity calibration.
    let sensor_a = slc::create_sensor(&db, "DepthSensorA", GLOBAL_PARAM_DEPTH_ID).await;
    slc::deploy_sensor(&db, sensor_a.id, SITE1_ID, slc::dt("2025-01-01T00:00:00Z")).await;
    let sensor_b = slc::create_sensor(&db, "DepthSensorB", GLOBAL_PARAM_DEPTH_ID).await;
    slc::deploy_sensor(&db, sensor_b.id, SITE_B_ID, slc::dt("2025-01-01T00:00:00Z")).await;
    let cal_a = sensor_a.identity_calibration_id;
    let cal_b = sensor_b.identity_calibration_id;
    let b_stream = slc::create_paired_stream(&db, "key-test-b-stats", SP_B_DEPTH_ID).await;

    let key = crate::common::seed_api_token(
        &db,
        crate::common::perms(true, true, false, false),
        Some(PROJECT_ID),
    )
    .await;
    let unscoped =
        crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;

    // search: the scoped key sees only Project-A sites for "Station", never "Station B".
    let (s, j) = crate::common::get_json_with_token(&app, "/api/search?q=Station", &key).await;
    assert_eq!(s, 200);
    let names: Vec<String> = j["results"]["sites"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|x| x["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !names.iter().any(|n| n == "Station B"),
        "scoped key must not see the Project-B site in search, got {names:?}"
    );
    // The unscoped key does see it (control).
    let (_s, j) =
        crate::common::get_json_with_token(&app, "/api/search?q=Station", &unscoped).await;
    let all_names: Vec<String> = j["results"]["sites"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|x| x["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        all_names.iter().any(|n| n == "Station B"),
        "unscoped key sees all projects"
    );

    // stream stats: in-project stream OK, cross-project stream 404 (not even existence is disclosed).
    let (s, _) = crate::common::get_with_token(
        &app,
        &format!("/api/streams/{A_DEPTH_STREAM_ID}/stats"),
        &key,
    )
    .await;
    assert_eq!(s, 200, "scoped key reads its own stream stats");
    let (s, _) =
        crate::common::get_with_token(&app, &format!("/api/streams/{b_stream}/stats"), &key).await;
    assert_eq!(
        s, 404,
        "scoped key must not read a cross-project stream's stats"
    );

    // sensor series + deployment bands: in-project sensor OK, cross-project sensor 404.
    for path in [
        format!("/api/sensors/{}/readings", sensor_a.id),
        format!("/api/sensors/{}/deployment_bands", sensor_a.id),
    ] {
        let (s, _) = crate::common::get_with_token(&app, &path, &key).await;
        assert_eq!(s, 200, "scoped key reads its own sensor at {path}");
    }
    for path in [
        format!("/api/sensors/{}/readings", sensor_b.id),
        format!("/api/sensors/{}/deployment_bands", sensor_b.id),
    ] {
        let (s, _) = crate::common::get_with_token(&app, &path, &key).await;
        assert_eq!(
            s, 404,
            "scoped key must not read a cross-project sensor at {path}"
        );
    }

    // calibration window: in-project calibration OK, cross-project 404.
    let (s, _) = crate::common::get_with_token(
        &app,
        &format!("/api/sensor_calibrations/{cal_a}/window"),
        &key,
    )
    .await;
    assert_eq!(s, 200, "scoped key reads its own calibration window");
    let (s, _) = crate::common::get_with_token(
        &app,
        &format!("/api/sensor_calibrations/{cal_b}/window"),
        &key,
    )
    .await;
    assert_eq!(
        s, 404,
        "scoped key must not read a cross-project calibration window"
    );

    // The unscoped key can read the Project-B sensor (control).
    let (s, _) = crate::common::get_with_token(
        &app,
        &format!("/api/sensors/{}/readings", sensor_b.id),
        &unscoped,
    )
    .await;
    assert_eq!(s, 200, "unscoped key reads any sensor");
}

#[tokio::test]
#[serial]
async fn api_token_use_is_recorded_in_audit_log() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (app, state) = crate::common::build_test_app_with_audit(db.clone());

    let key = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let tid = token_id(&db, &key).await;

    let (s, _) = crate::common::get_with_token(&app, "/api/sites", &key).await;
    assert_eq!(s, 200);

    // The audit write is fire-and-forget; poll briefly for it to land.
    let mut count: i64 = 0;
    for _ in 0..40 {
        let row = db
            .query_one(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT COUNT(*) AS c FROM api_token_audit_log WHERE token_id = $1",
                [tid.into()],
            ))
            .await
            .unwrap();
        count = row
            .and_then(|r| r.try_get::<i64>("", "c").ok())
            .unwrap_or(0);
        if count >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        count >= 1,
        "API-token use must be recorded in the audit log"
    );

    let row = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT method, path, status_code FROM api_token_audit_log \
             WHERE token_id = $1 ORDER BY created_at DESC LIMIT 1",
            [tid.into()],
        ))
        .await
        .unwrap()
        .expect("audit row");
    // The `/api` prefix is stripped by the router nest, so the audited path is the in-tier route.
    assert_eq!(row.try_get::<String>("", "method").unwrap(), "GET");
    assert_eq!(row.try_get::<String>("", "path").unwrap(), "/sites");
    assert_eq!(row.try_get::<i32>("", "status_code").unwrap(), 200);

    // The admin-only usage view (handler invoked against the shared state) surfaces the entry.
    let usage = river_db::routes::private::api_tokens::views::token_usage(State(state), Path(tid))
        .await
        .expect("usage ok")
        .0;
    assert!(
        usage
            .iter()
            .any(|e| e.path == "/sites" && e.method == "GET"),
        "usage view must include the recorded request"
    );
}
