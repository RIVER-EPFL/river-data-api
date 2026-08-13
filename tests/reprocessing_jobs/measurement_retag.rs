//! The `measurement_retag` job and its two triggering endpoints: readings in the scoped
//! sensors/streams are rewritten to the target measurement_type (decompression-safe txn), the
//! continuous aggregates are refreshed over the affected window (retagged spot rows drop out of the
//! rollups), and a rerun is an idempotent no-op.
//!
//! Run: cargo test --test reprocessing_jobs -- --test-threads=1

use river_db::common::AppEvent;
use river_db::routes::private::reprocessing_jobs::{job, worker};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

fn events() -> river_db::common::EventSender {
    tokio::sync::broadcast::channel::<AppEvent>(16).0
}

async fn make_stream(db: &DatabaseConnection, source_system: &str) -> Uuid {
    let stream_id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', '{source_system}', '{}', 'lab column', true)",
            Uuid::new_v4()
        ),
    )
    .await;
    stream_id
}

async fn insert_paired_reading(
    db: &DatabaseConnection,
    stream_id: Uuid,
    time: &str,
    value: f64,
    sensor_id: Option<Uuid>,
) {
    let sensor = sensor_id.map_or("NULL".to_string(), |s| format!("'{s}'"));
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings \
                (stream_id, site_id, parameter_id, time, replicate_index, raw_value, \
                 calibrated_value, sensor_id, logged, measurement_type, is_flagged) \
             VALUES ('{stream_id}', '{site}', '{param}', '{time}', 0, {value}, {value}, {sensor}, \
                     true, 'continuous', false)",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    )
    .await;
}

async fn count_where(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT count(*) AS n FROM {sql}"),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

async fn job_status(db: &DatabaseConnection, id: Uuid) -> String {
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT status FROM reprocessing_jobs WHERE id = '{id}'"),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<String>("", "status")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn retag_job_moves_scope_out_of_aggregates_and_reruns_idempotently() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let ev = events();
    let registry = job::build_registry();
    let wid = worker::worker_id();

    let stream_id = make_stream(&db, "labimport").await;
    insert_paired_reading(&db, stream_id, "2025-03-01T00:00:00Z", 10.0, None).await;
    insert_paired_reading(&db, stream_id, "2025-03-01T00:10:00Z", 20.0, None).await;
    // The shared helper only refreshes the seeded January window; cover this test's window.
    crate::common::exec(
        &db,
        "CALL refresh_continuous_aggregate('readings_hourly', '2025-02-25', '2025-03-05')",
    )
    .await;

    let hourly = format!(
        "readings_hourly WHERE site_id = '{}' AND parameter_id = '{}' \
         AND bucket = '2025-03-01T00:00:00Z'",
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TURB_ID
    );
    assert!(
        count_where(&db, &hourly).await > 0,
        "continuous rows roll up before the retag"
    );

    let id = worker::enqueue(
        &db,
        "measurement_retag",
        None,
        None,
        &serde_json::json!({ "stream_ids": [stream_id], "target": "spot" }),
        None,
    )
    .await
    .unwrap()
    .expect("enqueue inserts a row");
    worker::drain(&db, &ev, &registry, &wid).await.unwrap();

    assert_eq!(job_status(&db, id).await, "completed");
    assert_eq!(
        count_where(
            &db,
            &format!("readings WHERE stream_id = '{stream_id}' AND measurement_type = 'spot'")
        )
        .await,
        2,
        "both readings retagged"
    );
    assert_eq!(
        count_where(&db, &hourly).await,
        0,
        "spot rows drop out of the hourly rollup after the job's refresh"
    );

    // Idempotent rerun: nothing left to retag, still completes.
    let rerun = worker::enqueue(
        &db,
        "measurement_retag",
        None,
        None,
        &serde_json::json!({ "stream_ids": [stream_id], "target": "spot" }),
        None,
    )
    .await
    .unwrap()
    .expect("rerun enqueues");
    worker::drain(&db, &ev, &registry, &wid).await.unwrap();
    assert_eq!(job_status(&db, rerun).await, "completed");
}

#[tokio::test]
#[serial]
async fn sensors_retag_frequency_endpoint_updates_and_enqueues() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let ev = events();
    let registry = job::build_registry();
    let wid = worker::worker_id();

    let sensor_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!("INSERT INTO sensors (id, name) VALUES ('{sensor_id}', 'lab probe')"),
    )
    .await;
    let stream_id = make_stream(&db, "labimport").await;
    insert_paired_reading(&db, stream_id, "2025-03-02T00:00:00Z", 5.0, Some(sensor_id)).await;

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sensors/retag_frequency",
        &serde_json::json!({
            "sensor_ids": [sensor_id],
            "data_frequency": "low",
            "retag_existing": true,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "retag_frequency ({status}): {body}");
    assert_eq!(body["sensors_updated"], 1);
    assert!(body["job_id"].is_string(), "job enqueued: {body}");

    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT data_frequency FROM sensors WHERE id = '{sensor_id}'"),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<String>("", "data_frequency").unwrap(), "low");

    worker::drain(&db, &ev, &registry, &wid).await.unwrap();
    assert_eq!(
        count_where(
            &db,
            &format!("readings WHERE sensor_id = '{sensor_id}' AND measurement_type = 'spot'")
        )
        .await,
        1,
        "the sensor's existing reading was retagged"
    );

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sensors/retag_frequency",
        &serde_json::json!({"sensor_ids": [sensor_id], "data_frequency": "sometimes"}),
        &token,
    )
    .await;
    assert_eq!(status, 400, "invalid frequency ({status}): {body}");
}

#[tokio::test]
#[serial]
async fn streams_retag_endpoint_by_source_system() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let ev = events();
    let registry = job::build_registry();
    let wid = worker::worker_id();

    let s1 = make_stream(&db, "portalx").await;
    let s2 = make_stream(&db, "portalx").await;
    insert_paired_reading(&db, s1, "2025-03-03T00:00:00Z", 1.0, None).await;
    insert_paired_reading(&db, s2, "2025-03-03T00:10:00Z", 2.0, None).await;

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/retag",
        &serde_json::json!({
            "source_system": "portalx",
            "measurement_type": "spot",
            "retag_existing": true,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "streams retag ({status}): {body}");
    assert_eq!(body["streams_updated"], 2);

    assert_eq!(
        count_where(
            &db,
            "data_streams WHERE source_system = 'portalx' AND measurement_type = 'spot'"
        )
        .await,
        2,
        "stream classification stored for future ingestion"
    );

    worker::drain(&db, &ev, &registry, &wid).await.unwrap();
    assert_eq!(
        count_where(
            &db,
            &format!("readings WHERE stream_id IN ('{s1}', '{s2}') AND measurement_type = 'spot'")
        )
        .await,
        2,
        "existing readings across the source system retagged"
    );
}

#[tokio::test]
#[serial]
async fn retag_job_source_system_scope_flips_readings_and_refreshes_aggregates() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let ev = events();
    let registry = job::build_registry();
    let wid = worker::worker_id();

    let s1 = make_stream(&db, "portaly").await;
    let s2 = make_stream(&db, "portaly").await;
    let other = make_stream(&db, "vaisala").await;
    insert_paired_reading(&db, s1, "2025-03-04T00:00:00Z", 10.0, None).await;
    insert_paired_reading(&db, s2, "2025-03-04T00:10:00Z", 20.0, None).await;
    insert_paired_reading(&db, other, "2025-03-04T00:20:00Z", 30.0, None).await;
    crate::common::exec(
        &db,
        "CALL refresh_continuous_aggregate('readings_hourly', '2025-02-25', '2025-03-10')",
    )
    .await;

    let hourly = format!(
        "readings_hourly WHERE site_id = '{}' AND parameter_id = '{}' \
         AND bucket = '2025-03-04T00:00:00Z'",
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TURB_ID
    );
    assert!(
        count_where(&db, &hourly).await > 0,
        "all three roll up before the retag"
    );

    let id = worker::enqueue(
        &db,
        "measurement_retag",
        None,
        None,
        &serde_json::json!({ "source_system": "portaly", "target": "spot" }),
        None,
    )
    .await
    .unwrap()
    .expect("enqueue inserts a row");
    worker::drain(&db, &ev, &registry, &wid).await.unwrap();
    assert_eq!(job_status(&db, id).await, "completed");

    assert_eq!(
        count_where(
            &db,
            &format!("readings WHERE stream_id IN ('{s1}', '{s2}') AND measurement_type = 'spot'")
        )
        .await,
        2,
        "the source system's readings flip to spot"
    );
    assert_eq!(
        count_where(
            &db,
            &format!("readings WHERE stream_id = '{other}' AND measurement_type = 'spot'")
        )
        .await,
        0,
        "streams outside the source system are untouched"
    );

    // The job's aggregate refresh drops the retagged spot rows out of the rollup; the untouched
    // vaisala reading keeps the bucket alive with only its own contribution.
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT SUM(count)::bigint AS n FROM {hourly}"),
        ))
        .await
        .unwrap()
        .unwrap();
    let rolled_up: i64 = row.try_get("", "n").unwrap();
    assert_eq!(
        rolled_up, 1,
        "only the continuous reading remains in the refreshed rollup"
    );
}
