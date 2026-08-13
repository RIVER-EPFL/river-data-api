//! End-to-end sensor lifecycle over the HTTP API (the astroriver model): a sensor is deployed,
//! produces readings, is moved to another site, recalled, and explicitly reprocessed, and the
//! readings re-coordinate by time window automatically. Setup uses the proven `sensor_lifecycle`
//! DB helpers; the operations under test (move = new deployment, recall = PUT deployed_until,
//! manual reprocess) go through the real endpoints so the CrudCrate hooks + tracked jobs fire.
//!
//! Complements `reprocessing_test.rs` (which drives the same engine purely via SQL) by exercising
//! the HTTP surface WS2 wired up.
//!
//! Run: cargo test --test e2e -- --test-threads=1

use crate::common::e2e;
use crate::common::sensor_lifecycle as sl;
use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
#[serial]
async fn deploy_move_recall_reprocess_over_http() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let site1 = Uuid::parse_str(crate::common::SITE1_ID).unwrap();
    let site2 = Uuid::parse_str(crate::common::SITE2_ID).unwrap();

    // Setup: sensor with a corrective calibration (y = 2x + 1), deployed at site 1, producing
    // six readings across the first hour. (DB helpers, no hooks needed for setup.)
    let sensor = sl::create_sensor(&db, "lifecycle", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let cal = sl::add_calibration(&db, sensor.id, 2.0, 1.0, sl::dt("2025-06-01T00:00:00Z")).await;
    let dep_a = sl::deploy_sensor(
        &db,
        sensor.id,
        crate::common::SITE1_ID,
        sl::dt("2025-06-01T00:00:00Z"),
    )
    .await;
    let stream = sl::create_paired_stream(&db, "lifecycle", crate::common::PARAM_S1_TEMP_ID).await;
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
        dep_a,
        2.0,
        1.0,
        &raw,
    )
    .await;

    let rows = sl::get_readings(&db, stream).await;
    assert_eq!(rows.len(), 6);
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(
            r.calibrated_value,
            Some(2.0 * (10.0 + i as f64) + 1.0),
            "calibrated[{i}]"
        );
        assert_eq!(r.site_id, Some(site1), "all readings start at site 1");
    }

    // MOVE to site 2 at 00:30 via the API. The before_create hook closes the open site-1 deployment
    // at the new deployed_from; after_create spawns a tracked reprocessing job.
    let sensor_id = sensor.id.to_string();
    let _dep_b = e2e::create_deployment(
        &app,
        &token,
        &sensor_id,
        crate::common::SITE2_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "2025-06-01T00:30:00Z",
    )
    .await;
    assert!(
        sl::wait_for_reprocessing(&db, sensor.id, Duration::from_secs(30)).await,
        "reprocessing after move should complete"
    );

    // The old deployment was auto-closed exactly at the move instant.
    let until: chrono::DateTime<chrono::FixedOffset> = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT deployed_until FROM sensor_deployments WHERE id = $1",
            [dep_a.into()],
        ))
        .await
        .unwrap()
        .expect("dep_a row")
        .try_get("", "deployed_until")
        .expect("deployed_until should be set");
    assert_eq!(
        until.with_timezone(&chrono::Utc),
        sl::dt("2025-06-01T00:30:00Z"),
        "site-1 deployment closes at the move time"
    );

    // Readings re-coordinate by time window: < 00:30 stay at site 1, >= 00:30 move to site 2.
    // Calibrated values are unchanged (same calibration window still applies).
    let rows = sl::get_readings(&db, stream).await;
    for (i, r) in rows.iter().enumerate() {
        let expected_site = if i < 3 { site1 } else { site2 };
        assert_eq!(
            r.site_id,
            Some(expected_site),
            "reading[{i}] site after move"
        );
        assert_eq!(
            r.calibrated_value,
            Some(2.0 * (10.0 + i as f64) + 1.0),
            "calibrated[{i}] unchanged by move"
        );
    }

    // RECALL: end the open (site-2) deployment via PUT (the verb the UI's update uses). Routes through
    // after_update → tracked reprocessing.
    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/sensor_deployments/{_dep_b}"),
        &serde_json::json!({ "deployed_until": "2025-06-01T00:59:00Z" }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "recall (PUT) ({status}): {body}"
    );
    assert!(
        sl::wait_for_reprocessing(&db, sensor.id, Duration::from_secs(30)).await,
        "reprocessing after recall should complete"
    );

    // Manual reprocess endpoint (WS3): returns a tracked job that completes.
    let (status, repro) = crate::common::post_json_with_token(
        &app,
        "/api/actions/reprocess",
        &serde_json::json!({ "sensor_id": sensor_id }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "reprocess ({status}): {repro}"
    );
    let repro: serde_json::Value = serde_json::from_str(&repro).unwrap();
    let job_id = repro["job_id"].as_str().expect("reprocess returns job_id");
    assert_eq!(
        e2e::poll_job(&app, &token, job_id, 30).await,
        "completed",
        "manual reprocess job completes"
    );
}
