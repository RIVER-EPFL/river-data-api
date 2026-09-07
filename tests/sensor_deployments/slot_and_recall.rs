//! Editing a deployment window through the API: the start re-chains the previous deployment's
//! `deployed_until`, and a PATCH into an occupied slot is a clean client error while extending a
//! window the instrument already holds is not a self-conflict.
//!
//! The slot exclusion itself is `slot_boundaries.rs` and the recall's effect on attribution is
//! `lifecycle_rules.rs`; both assert more than this file did.
//!
//! Run: cargo test --test sensor_deployments -- --test-threads=1

use crate::common::e2e;
use crate::common::sensor_lifecycle as sl;
use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;
use uuid::Uuid;

async fn deployed_until(
    db: &sea_orm::DatabaseConnection,
    deployment_id: &str,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT deployed_until FROM sensor_deployments WHERE id = $1",
            [Uuid::parse_str(deployment_id).unwrap().into()],
        ))
        .await
        .unwrap()
        .expect("deployment row");
    row.try_get::<chrono::DateTime<chrono::FixedOffset>>("", "deployed_until")
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

#[tokio::test]
#[serial]
async fn editing_deployment_start_rechains_previous() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor = sl::create_sensor(&db, "mover", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let sensor_id = sensor.id.to_string();

    // Deploy at site 1 from 00:00, then move to site 2 at 02:00, the move auto-closes site 1 at 02:00.
    let dep1 = e2e::create_deployment(
        &app,
        &token,
        &sensor_id,
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "2025-06-01T00:00:00Z",
    )
    .await;
    let dep2 = e2e::create_deployment(
        &app,
        &token,
        &sensor_id,
        crate::common::SITE2_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "2025-06-01T02:00:00Z",
    )
    .await;
    assert_eq!(
        deployed_until(&db, &dep1).await,
        Some(sl::dt("2025-06-01T02:00:00Z")),
        "site-1 deployment closes when site-2 begins"
    );

    // Correct the move to 01:00, the previous deployment's end must follow.
    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/sensor_deployments/{dep2}"),
        &serde_json::json!({ "deployed_from": "2025-06-01T01:00:00Z" }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "edit deployed_from ({status}): {body}"
    );

    assert_eq!(
        deployed_until(&db, &dep1).await,
        Some(sl::dt("2025-06-01T01:00:00Z")),
        "editing the later deployment's start re-chains the earlier deployment's end"
    );
}

#[tokio::test]
#[serial]
async fn patch_into_occupied_slot_is_a_clean_client_error() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor_a = sl::create_sensor(&db, "patch-a", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let sensor_b = sl::create_sensor(&db, "patch-b", crate::common::GLOBAL_PARAM_TEMP_ID).await;

    // A holds (site 1, Temperature); B holds (site 2, Temperature), different slots, both allowed.
    let _dep_a = e2e::create_deployment(
        &app,
        &token,
        &sensor_a.id.to_string(),
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "2025-06-01T00:00:00Z",
    )
    .await;
    let dep_b = e2e::create_deployment(
        &app,
        &token,
        &sensor_b.id.to_string(),
        crate::common::SITE2_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "2025-06-01T00:00:00Z",
    )
    .await;

    // Moving B into A's slot via PATCH must surface the `before_update` pre-check as a clean 400,
    // not the raw `excl_deployment_site_param_slot` 500 the path produced before the hook existed.
    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/sensor_deployments/{dep_b}"),
        &serde_json::json!({ "site_id": crate::common::SITE1_ID }),
        &token,
    )
    .await;
    assert_eq!(
        status, 400,
        "moving into an occupied slot must be a clean 400, got {status}: {body}"
    );
}

#[tokio::test]
#[serial]
async fn patch_window_extension_is_not_a_self_conflict() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor = sl::create_sensor(&db, "solo", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let dep = e2e::create_deployment(
        &app,
        &token,
        &sensor.id.to_string(),
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "2025-06-01T06:00:00Z",
    )
    .await;

    // Pulling the only deployment's start earlier must not conflict with itself (self-exclusion).
    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/sensor_deployments/{dep}"),
        &serde_json::json!({ "deployed_from": "2025-06-01T00:00:00Z" }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "extending own window must succeed ({status}): {body}"
    );
    assert_eq!(
        deployed_until(&db, &dep).await,
        None,
        "the deployment stays open after extending its start"
    );
}
