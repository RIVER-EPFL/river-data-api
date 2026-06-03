//! `POST /api/actions/reprocess_all` (the backdate operation) re-derives site/deployment attribution
//! for every (site, parameter) slot from the deployment timeline — re-owning readings that were
//! unattributed (site_id NULL) but fall within a deployment window.
//!
//! Run: cargo test --test reprocess_all_test -- --test-threads=1

mod common;

use common::e2e;
use common::sensor_lifecycle as sl;
use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;
use uuid::Uuid;

#[tokio::test]
#[serial]
async fn reprocess_all_reowns_unattributed_readings() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());
    let site1 = Uuid::parse_str(common::SITE1_ID).unwrap();
    let temp = common::GLOBAL_PARAM_TEMP_ID;

    let sensor = sl::create_sensor(&db, "backdate", temp).await;
    let _cal = sl::add_calibration(&db, sensor.id, 1.0, 0.0, sl::dt("2025-06-01T00:00:00Z")).await;
    let dep = sl::deploy_sensor(&db, sensor.id, common::SITE1_ID, sl::dt("2025-06-01T00:00:00Z")).await;
    let stream = sl::create_paired_stream(&db, "backdate", common::PARAM_S1_TEMP_ID).await;

    // Readings carry the sensor + parameter but no site/deployment, yet fall inside dep's window.
    for (i, t) in ["2025-06-01T00:15:00Z", "2025-06-01T00:45:00Z"].iter().enumerate() {
        db.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "INSERT INTO readings (stream_id, time, raw_value, parameter_id, sensor_id, replicate_index) \
                 VALUES ('{stream}', '{t}', {}, '{temp}', '{}', 0)",
                10.0 + i as f64,
                sensor.id
            ),
        ))
        .await
        .unwrap();
    }

    let (status, body) = common::post_json_with_token(
        &app,
        "/api/actions/reprocess_all",
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "reprocess_all ({status}): {body}");
    let resp: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(resp["slots"].as_u64().unwrap() >= 1, "at least one slot queued: {body}");
    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "reprocess_all", 30).await,
        "the reprocess_all job completes"
    );

    let rows = sl::get_readings_for_sensor(&db, sensor.id).await;
    assert_eq!(rows.len(), 2, "both readings remain");
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(r.site_id, Some(site1), "reading[{i}] re-owned to site1");
        assert_eq!(r.deployment_id, Some(dep), "reading[{i}] re-owned to the deployment");
    }
}
