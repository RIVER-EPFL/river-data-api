//! Deployment-as-the-twin-of-calibration behaviours introduced in the sensor-lifecycle work:
//! the `(site, parameter)` slot is hard-enforced (one sensor at a time), editing a deployment's
//! start re-chains the previous deployment's `deployed_until`, and recalling a sensor un-attributes
//! the readings logged after the recall while leaving earlier ones in place.
//!
//! Run: cargo test --test e2e -- --test-threads=1

use crate::common::e2e;
use crate::common::sensor_lifecycle as sl;
use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;
use std::time::Duration;
use uuid::Uuid;

async fn deployed_until(
    db: &sea_orm::DatabaseConnection,
    deployment_id: &str,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let row = db
        .query_one(Statement::from_sql_and_values(
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
async fn slot_exclusion_rejects_a_second_sensor() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    // Two distinct sensors measuring the SAME parameter.
    let sensor_a = sl::create_sensor(&db, "slot-a", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let sensor_b = sl::create_sensor(&db, "slot-b", crate::common::GLOBAL_PARAM_TEMP_ID).await;

    // A takes the (site 1, Temperature) slot.
    let _dep_a = e2e::create_deployment(
        &app,
        &token,
        &sensor_a.id.to_string(),
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "2025-06-01T00:00:00Z",
    )
    .await;

    // B cannot occupy the same slot over an overlapping window.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/sensor_deployments",
        &serde_json::json!({
            "sensor_id": sensor_b.id,
            "site_id": crate::common::SITE1_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
            "deployed_from": "2025-06-01T06:00:00Z"
        }),
        &token,
    )
    .await;
    assert!(
        status >= 400,
        "second sensor in an occupied slot must be rejected; got {status}: {body}"
    );

    let count: i64 = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*) AS c FROM sensor_deployments \
             WHERE site_id = $1 AND parameter_id = $2 AND deployed_until IS NULL",
            [
                Uuid::parse_str(crate::common::SITE1_ID).unwrap().into(),
                Uuid::parse_str(crate::common::GLOBAL_PARAM_TEMP_ID)
                    .unwrap()
                    .into(),
            ],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "c")
        .unwrap();
    assert_eq!(count, 1, "only one sensor may hold the slot");
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
async fn recall_unattributes_post_recall_readings() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let site1 = Uuid::parse_str(crate::common::SITE1_ID).unwrap();

    let sensor = sl::create_sensor(&db, "recall", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let cal = sl::add_calibration(&db, sensor.id, 1.0, 0.0, sl::dt("2025-06-01T00:00:00Z")).await;
    let dep = sl::deploy_sensor(
        &db,
        sensor.id,
        crate::common::SITE1_ID,
        sl::dt("2025-06-01T00:00:00Z"),
    )
    .await;
    let stream = sl::create_paired_stream(&db, "recall", crate::common::PARAM_S1_TEMP_ID).await;
    let raw: Vec<(_, f64)> = (0..6)
        .map(|i| {
            (
                sl::dt(&format!("2025-06-01T00:{:02}:00Z", i * 10)),
                10.0 + i as f64,
            )
        })
        .collect();
    sl::insert_readings(
        &db,
        stream,
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        sensor.id,
        cal,
        dep,
        1.0,
        0.0,
        &raw,
    )
    .await;

    // Recall the sensor at 00:30, everything from 00:30 onward was logged with the sensor pulled out.
    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/sensor_deployments/{dep}"),
        &serde_json::json!({ "deployed_until": "2025-06-01T00:30:00Z" }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "recall ({status}): {body}");
    assert!(
        sl::wait_for_reprocessing(&db, sensor.id, Duration::from_secs(30)).await,
        "reprocessing after recall should complete"
    );

    let rows = sl::get_readings(&db, stream).await;
    assert_eq!(rows.len(), 6);
    for (i, r) in rows.iter().enumerate() {
        if i < 3 {
            assert_eq!(
                r.site_id,
                Some(site1),
                "reading[{i}] before recall stays at the site"
            );
            assert!(
                r.deployment_id.is_some(),
                "reading[{i}] before recall keeps its deployment"
            );
        } else {
            assert_eq!(
                r.site_id, None,
                "reading[{i}] after recall is un-attributed"
            );
            assert_eq!(
                r.deployment_id, None,
                "reading[{i}] after recall has no deployment"
            );
        }
    }
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
