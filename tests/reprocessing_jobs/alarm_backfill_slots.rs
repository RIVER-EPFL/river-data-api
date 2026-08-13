//! The `alarm_backfill` job's slot-list shape: when `params.slots` carries `[site_id, parameter_id]`
//! pairs, the job loops `evaluate_alarm_episodes` over each pair with the shared `start`/`end`
//! window, the per-slot path the inline batch/CSV ingest spawns used before they were flipped to
//! `worker::enqueue`.
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

/// A data stream the test readings hang off (readings.stream_id has a FK to data_streams).
async fn make_stream(db: &DatabaseConnection) -> Uuid {
    let stream_id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', 'test', '{}', 'turbidity', true)",
            Uuid::new_v4()
        ),
    )
    .await;
    stream_id
}

/// Insert a reading at SITE1/Turbidity (site_id/parameter_id set so the episode evaluator can scope
/// it). `value` above 100 is a warning, above 500 an alarm.
async fn insert_turbidity(db: &DatabaseConnection, stream_id: Uuid, time: &str, value: f64) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings \
                (stream_id, site_id, parameter_id, time, replicate_index, raw_value, calibrated_value, \
                 logged, measurement_type, is_flagged) \
             VALUES ('{stream_id}', '{site}', '{param}', '{time}', 0, {value}, {value}, \
                     false, 'continuous', false)",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    )
    .await;
}

async fn turbidity_episode_count(db: &DatabaseConnection) -> i64 {
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT count(*) AS n FROM alarm_events WHERE site_id = '{site}' AND parameter_id = '{param}'",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn alarm_backfill_with_slots_reconstructs_episodes_per_pair() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    // Seeds SITE1/Turbidity site_parameter + the global threshold (warning > 100, alarm > 500).
    crate::common::seed_test_data(&db).await;
    let ev = events();
    let registry = job::build_registry();
    let wid = worker::worker_id();

    // ok → warning → ok (resolves) → alarm → ok (resolves): two resolved episodes.
    let stream_id = make_stream(&db).await;
    insert_turbidity(&db, stream_id, "2025-03-01T00:00:00Z", 50.0).await;
    insert_turbidity(&db, stream_id, "2025-03-01T00:10:00Z", 150.0).await;
    insert_turbidity(&db, stream_id, "2025-03-01T00:20:00Z", 40.0).await;
    insert_turbidity(&db, stream_id, "2025-03-01T00:30:00Z", 600.0).await;
    insert_turbidity(&db, stream_id, "2025-03-01T00:40:00Z", 30.0).await;

    assert_eq!(
        turbidity_episode_count(&db).await,
        0,
        "no episodes before the backfill"
    );

    let id = worker::enqueue(
        &db,
        "alarm_backfill",
        None,
        None,
        &serde_json::json!({
            "slots": [[crate::common::SITE1_ID, crate::common::GLOBAL_PARAM_TURB_ID]],
            "start": "2025-03-01T00:00:00Z",
            "end": "2025-03-01T01:00:00Z",
        }),
        None,
    )
    .await
    .unwrap()
    .expect("enqueue inserts a row");

    worker::drain(&db, &ev, &registry, &wid).await.unwrap();

    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT status FROM reprocessing_jobs WHERE id = '{id}'"),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<String>("", "status").unwrap(), "completed");

    assert_eq!(
        turbidity_episode_count(&db).await,
        2,
        "the slot path reconstructs one warning and one alarm episode for the pair"
    );
}

#[tokio::test]
#[serial]
async fn alarm_backfill_with_slots_requires_window() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let ev = events();
    let registry = job::build_registry();
    let wid = worker::worker_id();

    // slots present but no start/end → the job errors (the inline path always passed a window).
    let id = worker::enqueue(
        &db,
        "alarm_backfill",
        None,
        None,
        &serde_json::json!({
            "slots": [[crate::common::SITE1_ID, crate::common::GLOBAL_PARAM_TURB_ID]],
        }),
        None,
    )
    .await
    .unwrap()
    .unwrap();

    worker::drain(&db, &ev, &registry, &wid).await.unwrap();

    let status: String = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT status FROM reprocessing_jobs WHERE id = '{id}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "status")
        .unwrap();
    assert_eq!(status, "failed", "slots without a window is a job error");
}
