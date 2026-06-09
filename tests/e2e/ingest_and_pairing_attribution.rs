//! Write-time attribution: when a deployment already covers a reading's (site, parameter, time), the
//! ingest paths now stamp `sensor_id`/`deployment_id`/`calibration_id` at write time instead of
//! leaving them NULL (the source of historical orphans). Readings before the deployment stay NULL
//! (they need a backdate). Pairing a stream into a slot attributes the backfilled readings by the
//! deployment window, not a single frozen sensor.
//!
//! Run: cargo test --test e2e -- --test-threads=1


use crate::common::e2e;
use crate::common::sensor_lifecycle as sl;
use sea_orm::{ConnectionTrait, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

/// (sensor_id, deployment_id, calibration_id) for the reading at an exact time at the site/param.
async fn attr_at(
    db: &sea_orm::DatabaseConnection,
    time: &str,
) -> (Option<Uuid>, Option<Uuid>, Option<Uuid>) {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sensor_id, deployment_id, calibration_id FROM readings \
             WHERE site_id = $1::uuid AND parameter_id = $2::uuid AND time = $3",
            [
                crate::common::SITE1_ID.into(),
                crate::common::GLOBAL_PARAM_TEMP_ID.into(),
                sl::dt(time).into(),
            ],
        ))
        .await
        .unwrap()
        .expect("reading exists");
    (
        row.try_get("", "sensor_id").ok(),
        row.try_get("", "deployment_id").ok(),
        row.try_get("", "calibration_id").ok(),
    )
}

#[tokio::test]
#[serial]
async fn batch_attributes_rows_inside_the_deployment_window() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor = sl::create_sensor(&db, "ingest-probe", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let dep = sl::deploy_sensor(&db, sensor.id, crate::common::SITE1_ID, sl::dt("2025-06-01T00:00:00Z")).await;

    // Batch with no sensor_id: one row inside the window, one before it.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &json!({
            "readings": [
                { "site_id": crate::common::SITE1_ID, "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID, "time": "2025-07-01T00:00:00Z", "raw_value": 21.0 },
                { "site_id": crate::common::SITE1_ID, "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID, "time": "2025-05-01T00:00:00Z", "raw_value": 19.0 }
            ]
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "batch insert ({status}): {body}");

    // In-window row attributed to the sensor + its deployment + identity calibration.
    let (sid, did, cid) = attr_at(&db, "2025-07-01T00:00:00Z").await;
    assert_eq!(sid, Some(sensor.id), "in-window row gets sensor");
    assert_eq!(did, Some(dep), "in-window row gets deployment");
    assert_eq!(cid, Some(sensor.identity_calibration_id), "in-window row gets calibration");

    // Pre-deployment row stays unattributed (needs a backdate).
    let (sid_before, _, _) = attr_at(&db, "2025-05-01T00:00:00Z").await;
    assert_eq!(sid_before, None, "pre-deployment row stays NULL");
}

#[tokio::test]
#[serial]
async fn pairing_attributes_by_deployment_window() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    // The slot already has sensor A deployed (open). Pairing must attribute to A by window — not to
    // the sensor the pairing path auto-creates for the stream.
    let a = sl::create_sensor(&db, "incumbent-A", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let dep_a = sl::deploy_sensor(&db, a.id, crate::common::SITE1_ID, sl::dt("2025-06-01T00:00:00Z")).await;

    let stream = sl::create_unpaired_stream(&db, "to-pair").await;
    sl::insert_unpaired_readings(
        &db,
        stream,
        &[
            (sl::dt("2025-07-01T00:00:00Z"), 21.0),
            (sl::dt("2025-08-01T00:00:00Z"), 22.0),
        ],
    )
    .await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream}/pair"),
        &json!({ "site_parameter_id": crate::common::PARAM_S1_TEMP_ID }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "pair ({status}): {body}");
    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "pairing_backfill", 30).await,
        "pairing window-reprocess completes"
    );

    let rows = sl::get_readings(&db, stream).await;
    assert_eq!(rows.len(), 2, "both readings present");
    for r in &rows {
        assert_eq!(r.site_id.map(|s| s.to_string()), Some(crate::common::SITE1_ID.to_string()));
        assert_eq!(r.sensor_id, Some(a.id), "reading {} attributed to the slot's deployed sensor", r.time);
        assert_eq!(r.deployment_id, Some(dep_a), "reading {} gets A's deployment", r.time);
    }
}
