//! Rollback reopens the previous deployment. When a sensor is moved (deployment A at site1 closed,
//! deployment B opened at site2) and B is rolled back, B's-window readings must revert to site1 /
//! deployment A — not get un-attributed. `recompute_deployed_until` only shortens windows, so the
//! handler explicitly reopens the previous deployment to the boundary B vacated.
//!
//! Run: cargo test --test e2e_rollback_deployment_test -- --test-threads=1

mod common;

use common::sensor_lifecycle as sl;
use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;
use uuid::Uuid;

async fn deployed_until(db: &sea_orm::DatabaseConnection, id: Uuid) -> Option<chrono::DateTime<chrono::Utc>> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT deployed_until FROM sensor_deployments WHERE id = $1",
            [id.into()],
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
async fn rollback_reopens_previous_deployment() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());

    let site1 = Uuid::parse_str(common::SITE1_ID).unwrap();

    let sensor = sl::create_sensor(&db, "rollback", common::GLOBAL_PARAM_TEMP_ID).await;
    let cal = sl::add_calibration(&db, sensor.id, 1.0, 0.0, sl::dt("2025-06-01T00:00:00Z")).await;

    // Post-move state: A at site1 [00:00, 02:00), B at site2 [02:00, ∞).
    let dep_a = sl::deploy_sensor(&db, sensor.id, common::SITE1_ID, sl::dt("2025-06-01T00:00:00Z")).await;
    sl::end_deployment(&db, dep_a, sl::dt("2025-06-01T02:00:00Z")).await;
    let dep_b = sl::deploy_sensor(&db, sensor.id, common::SITE2_ID, sl::dt("2025-06-01T02:00:00Z")).await;

    let stream = sl::create_paired_stream(&db, "rollback", common::PARAM_S1_TEMP_ID).await;
    sl::insert_readings(
        &db, stream, common::SITE1_ID, common::GLOBAL_PARAM_TEMP_ID, sensor.id, cal, dep_a, 1.0, 0.0,
        &[
            (sl::dt("2025-06-01T00:30:00Z"), 10.0),
            (sl::dt("2025-06-01T01:00:00Z"), 11.0),
            (sl::dt("2025-06-01T01:30:00Z"), 12.0),
        ],
    )
    .await;
    sl::insert_readings(
        &db, stream, common::SITE2_ID, common::GLOBAL_PARAM_TEMP_ID, sensor.id, cal, dep_b, 1.0, 0.0,
        &[
            (sl::dt("2025-06-01T02:30:00Z"), 13.0),
            (sl::dt("2025-06-01T03:00:00Z"), 14.0),
            (sl::dt("2025-06-01T03:30:00Z"), 15.0),
        ],
    )
    .await;

    let (status, body) = common::post_json_with_token(
        &app,
        "/api/actions/rollback_deployment",
        &serde_json::json!({ "deployment_id": dep_b }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "rollback ({status}): {body}");
    let resp: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        resp["readings_reassigned"].as_u64(),
        Some(3),
        "B's three readings are reassigned: {body}"
    );
    assert_eq!(resp["previous_deployment_id"].as_str(), Some(dep_a.to_string().as_str()));

    // A reopened to open-ended (B was open), absorbing B's vacated window.
    assert_eq!(
        deployed_until(&db, dep_a).await,
        None,
        "deployment A reopened (deployed_until NULL)"
    );

    // rollback_deployment reprocesses synchronously, so the readings are already re-attributed.
    let rows = sl::get_readings_for_sensor(&db, sensor.id).await;
    assert_eq!(rows.len(), 6, "all six readings remain");
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(r.site_id, Some(site1), "reading[{i}] reverts to site1");
        assert_eq!(r.deployment_id, Some(dep_a), "reading[{i}] reverts to deployment A");
    }

    let open_count: i64 = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*) AS c FROM sensor_deployments WHERE sensor_id = $1 AND deployed_until IS NULL",
            [sensor.id.into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "c")
        .unwrap();
    assert_eq!(open_count, 1, "exactly one open deployment after rollback");
}
