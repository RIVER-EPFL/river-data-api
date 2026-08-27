//! The `replicate_reindex_repair` job: discard a replicate-family stream's readings, rewind its
//! sync cursor and ask the owning sync service to send them again. Refused whole when a targeted
//! stream is paired or holds a flagged reading.
//!
//! Run: cargo test --test reprocessing_jobs replicate_reindex_repair -- --test-threads=1

use river_db::common::AppEvent;
use river_db::routes::private::reprocessing_jobs::{job, worker};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

const SOURCE: &str = "cnet";
const FAMILY_KEY: &str = "STA:DOC_avg_ppb:reps";
const T1: &str = "2025-03-01T08:00:00Z";
const T2: &str = "2025-03-01T09:00:00Z";

fn events() -> river_db::common::EventSender {
    tokio::sync::broadcast::channel::<AppEvent>(16).0
}

async fn seed_family(db: &DatabaseConnection) -> Uuid {
    let id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active, \
                                       measurement_type, last_data_time, metadata) \
             VALUES ('{id}', '{SOURCE}', '{FAMILY_KEY}', 'DOC replicates', true, 'spot', \
                     '{T2}', \
                     '{{\"replicates\": {{\"source_columns\": \
                       [\"DOC_1_ppb\", \"DOC_2_ppb\", \"DOC_3_ppb\"], \
                       \"portal_mean_column\": \"DOC_avg_ppb\"}}}}'::jsonb)"
        ),
    )
    .await;
    for time in [T1, T2] {
        for (idx, value) in [(0, 10.0), (1, 20.0)] {
            crate::common::exec(
                db,
                &format!(
                    "INSERT INTO readings (stream_id, time, replicate_index, raw_value, \
                                           measurement_type) \
                     VALUES ('{id}', '{time}', {idx}, {value}, 'spot')"
                ),
            )
            .await;
        }
    }
    id
}

async fn setup() -> (DatabaseConnection, Uuid) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::sensor_lifecycle::seed_base_entities(&db).await;
    let stream = seed_family(&db).await;
    (db, stream)
}

async fn seed_service(db: &DatabaseConnection) -> Uuid {
    let id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO sync_services (id, service_type, instance_id, status, last_heartbeat, \
                                        created_at, updated_at) \
             VALUES ('{id}', '{SOURCE}', 'cnet-1', 'registered', now(), now(), now())"
        ),
    )
    .await;
    id
}

async fn run_job(db: &DatabaseConnection, params: serde_json::Value) -> Uuid {
    let ev = events();
    let registry = job::build_registry();
    let wid = worker::worker_id();
    let id = worker::enqueue(db, "replicate_reindex_repair", None, None, &params, None)
        .await
        .unwrap()
        .expect("enqueue inserts a row");
    worker::drain(db, &ev, &registry, &wid).await.unwrap();
    id
}

async fn job_outcome(db: &DatabaseConnection, id: Uuid) -> (String, serde_json::Value, String) {
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT status, COALESCE(detail, '{{}}'::jsonb) AS detail, \
                        COALESCE(error_message, '') AS error_message \
                 FROM reprocessing_jobs WHERE id = '{id}'"
            ),
        ))
        .await
        .unwrap()
        .unwrap();
    (
        row.try_get::<String>("", "status").unwrap(),
        row.try_get::<serde_json::Value>("", "detail").unwrap(),
        row.try_get::<String>("", "error_message").unwrap(),
    )
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    crate::common::e2e::count(db, sql).await
}

#[tokio::test]
#[serial]
async fn readings_are_discarded_the_cursor_rewound_and_a_resync_issued() {
    let (db, stream) = setup().await;
    let service = seed_service(&db).await;

    let id = run_job(&db, serde_json::json!({"source_system": SOURCE})).await;
    let (status, detail, error) = job_outcome(&db, id).await;
    assert_eq!(status, "completed", "{error} / detail: {detail}");
    assert_eq!(detail["counts"]["streams_repaired"], 1, "detail: {detail}");
    assert_eq!(detail["counts"]["readings_deleted"], 4, "detail: {detail}");
    assert_eq!(detail["counts"]["commands_issued"], 1, "detail: {detail}");

    assert_eq!(
        count(
            &db,
            &format!("SELECT COUNT(*) FROM readings WHERE stream_id = '{stream}'")
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM data_streams \
                 WHERE id = '{stream}' AND last_data_time IS NULL"
            )
        )
        .await,
        1
    );

    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT command, payload FROM sync_commands WHERE service_id = '{service}'"),
        ))
        .await
        .unwrap()
        .expect("a command was issued");
    assert_eq!(
        row.try_get::<String>("", "command").unwrap(),
        "resync_streams"
    );
    let payload: serde_json::Value = row.try_get("", "payload").unwrap();
    assert_eq!(payload["source_keys"][0], FAMILY_KEY);
    assert_eq!(payload["overwrite"], false);
}

#[tokio::test]
#[serial]
async fn a_dry_run_deletes_nothing() {
    let (db, stream) = setup().await;
    seed_service(&db).await;

    let id = run_job(
        &db,
        serde_json::json!({"stream_ids": [stream.to_string()], "dry_run": true}),
    )
    .await;
    let (status, detail, error) = job_outcome(&db, id).await;
    assert_eq!(status, "completed", "{error} / detail: {detail}");
    assert_eq!(detail["counts"]["streams_repaired"], 0, "detail: {detail}");
    assert_eq!(detail["counts"]["commands_issued"], 0, "detail: {detail}");
    assert_eq!(detail["streams"][0]["status"], "would_repair");
    assert_eq!(
        count(
            &db,
            &format!("SELECT COUNT(*) FROM readings WHERE stream_id = '{stream}'")
        )
        .await,
        4
    );
}

#[tokio::test]
#[serial]
async fn a_paired_stream_fails_the_run_and_keeps_its_readings() {
    let (db, stream) = setup().await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE data_streams SET site_parameter_id = '{}', paired_at = now() \
             WHERE id = '{stream}'",
            crate::common::PARAM_S1_TEMP_ID
        ),
    )
    .await;

    let id = run_job(&db, serde_json::json!({"source_system": SOURCE})).await;
    let (status, _detail, error) = job_outcome(&db, id).await;
    assert_eq!(status, "failed");
    assert!(
        error.contains(FAMILY_KEY),
        "error names the stream: {error}"
    );
    assert_eq!(
        count(
            &db,
            &format!("SELECT COUNT(*) FROM readings WHERE stream_id = '{stream}'")
        )
        .await,
        4
    );
}

#[tokio::test]
#[serial]
async fn a_flagged_reading_fails_the_run_and_keeps_its_readings() {
    let (db, stream) = setup().await;
    crate::common::exec(
        &db,
        &format!("UPDATE readings SET is_flagged = true WHERE stream_id = '{stream}' AND time = '{T1}' AND replicate_index = 1"),
    )
    .await;

    let id = run_job(&db, serde_json::json!({"source_system": SOURCE})).await;
    let (status, _detail, error) = job_outcome(&db, id).await;
    assert_eq!(status, "failed");
    assert!(
        error.contains(FAMILY_KEY),
        "error names the stream: {error}"
    );
    assert_eq!(
        count(
            &db,
            &format!("SELECT COUNT(*) FROM readings WHERE stream_id = '{stream}'")
        )
        .await,
        4
    );
}

#[tokio::test]
#[serial]
async fn a_stream_outside_the_scope_is_untouched() {
    let (db, stream) = setup().await;
    seed_service(&db).await;

    let id = run_job(
        &db,
        serde_json::json!({"stream_ids": [Uuid::new_v4().to_string()]}),
    )
    .await;
    let (status, _detail, error) = job_outcome(&db, id).await;
    assert_eq!(status, "failed");
    assert!(error.contains("no replicate family streams"), "{error}");
    assert_eq!(
        count(
            &db,
            &format!("SELECT COUNT(*) FROM readings WHERE stream_id = '{stream}'")
        )
        .await,
        4
    );
}
