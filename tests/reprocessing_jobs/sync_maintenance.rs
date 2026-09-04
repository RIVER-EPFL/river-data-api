//! The two scheduled sync-ledger Services, both of which run unattended and one of which deletes
//! rows: `sync_full_reassert` queues a full-sync command for each live unpaused service of the
//! configured types, and `sync_ledger_retention` prunes the two ledgers by age.
//!
//! Run: cargo test --test reprocessing_jobs -- --test-threads=1

use river_db::common::AppEvent;
use river_db::routes::private::reprocessing_jobs::job::{Job, JobRegistry};
use river_db::routes::private::reprocessing_jobs::jobs::{SyncFullReassert, SyncLedgerRetention};
use river_db::routes::private::reprocessing_jobs::worker;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use std::sync::Arc;
use uuid::Uuid;

fn events() -> river_db::common::EventSender {
    tokio::sync::broadcast::channel::<AppEvent>(16).0
}

async fn exec(db: &DatabaseConnection, sql: &str) {
    db.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_owned(),
    ))
    .await
    .unwrap();
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_owned(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "c")
    .unwrap()
}

/// Run one registered job to completion through the worker pool, returning what it reported.
async fn run_job(db: &DatabaseConnection, job: Arc<dyn Job>) -> i64 {
    let name = job.name();
    let mut registry = JobRegistry::new();
    registry.register(job);
    let job_id = worker::enqueue(db, name, None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .expect("a fresh enqueue inserts a row");
    let ev = events();
    let wid = worker::worker_id();
    assert!(
        worker::run_one(db, &ev, &registry, &wid).await.unwrap(),
        "{name} was claimed and run"
    );
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT status, COALESCE(readings_updated, -1)::bigint AS c \
                 FROM reprocessing_jobs WHERE id = '{job_id}'"
            ),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.try_get::<String>("", "status").unwrap(),
        "completed",
        "{name} completed"
    );
    row.try_get::<i64>("", "c").unwrap()
}

async fn seed_service(
    db: &DatabaseConnection,
    service_type: &str,
    heartbeat_age: &str,
    paused: bool,
) -> Uuid {
    let id = Uuid::new_v4();
    exec(
        db,
        &format!(
            "INSERT INTO sync_services \
               (id, service_type, instance_id, status, paused, full_reassert_enabled, \
                last_heartbeat, created_at, updated_at) \
             VALUES ('{id}', '{service_type}', '{id}', 'registered', {paused}, true, \
                     NOW() - INTERVAL '{heartbeat_age}', NOW(), NOW())"
        ),
    )
    .await;
    id
}

async fn pending_commands(db: &DatabaseConnection, service_id: Uuid) -> i64 {
    count(
        db,
        &format!(
            "SELECT count(*) AS c FROM sync_commands \
             WHERE service_id = '{service_id}' AND command = 'trigger_full_sync' AND status = 'pending'"
        ),
    )
    .await
}

/// Scenario: the weekly re-assert runs against a service that has gone quiet and one that has not.
/// Expected behaviour: a service whose heartbeat is over an hour old is not queued, and a second
/// run queues nothing while the first command is still pending. The opt-in flag and the paused
/// case are `sync_full_reassert.rs`.
#[tokio::test]
#[serial]
async fn full_reassert_skips_a_silent_service_and_never_queues_twice() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let live = seed_service(&db, "rshiny", "1 minute", false).await;
    let stale = seed_service(&db, "rshiny", "3 hours", false).await;

    let config = crate::common::test_config();
    let queued = run_job(&db, Arc::new(SyncFullReassert::from_config(&config))).await;

    assert_eq!(queued, 1, "one command queued");
    assert_eq!(pending_commands(&db, live).await, 1, "the live service");
    assert_eq!(
        pending_commands(&db, stale).await,
        0,
        "not one whose heartbeat is an hour old"
    );

    let requeued = run_job(&db, Arc::new(SyncFullReassert::from_config(&config))).await;
    assert_eq!(requeued, 0, "a pending command is not queued twice");
    assert_eq!(pending_commands(&db, live).await, 1);

    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: the daily ledger prune runs over old and recent rows.
/// Expected behaviour: rows past their retention go, recent rows stay, and a `running` sync event
/// is never deleted however old it is (the staleness sweep owns those).
#[tokio::test]
#[serial]
async fn ledger_retention_prunes_by_age_and_never_a_running_event() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    let service = seed_service(&db, "rshiny", "1 minute", false).await;
    for (label, age, status) in [
        ("old", "120 days", "completed"),
        ("recent", "1 day", "completed"),
        ("old_running", "120 days", "running"),
    ] {
        exec(
            &db,
            &format!(
                "INSERT INTO sync_events (id, service_id, event_type, status, started_at) \
                 VALUES (gen_random_uuid(), '{service}', '{label}', '{status}', \
                         NOW() - INTERVAL '{age}')"
            ),
        )
        .await;
    }

    let stream = Uuid::new_v4();
    exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active) \
             VALUES ('{stream}', 'ledger-retention', '{stream}', true)"
        ),
    )
    .await;
    for age in ["400 days", "10 days"] {
        exec(
            &db,
            &format!(
                "INSERT INTO ingest_receipts \
                   (stream_id, at, submitted, new_rows, changed, unchanged, retained, \
                    rejected_total, rejected, dropped, withdrawn) \
                 VALUES ('{stream}', NOW() - INTERVAL '{age}', 1, 1, 0, 0, 0, 0, '{{}}'::jsonb, 0, 0)"
            ),
        )
        .await;
    }

    let config = crate::common::test_config();
    assert_eq!(config.sync_event_retention_days, 90);
    assert_eq!(config.ingest_receipt_retention_days, 365);

    let pruned = run_job(&db, Arc::new(SyncLedgerRetention::from_config(&config))).await;
    assert_eq!(pruned, 2, "one event and one receipt");

    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM sync_events WHERE event_type = 'old'"
        )
        .await,
        0,
        "the aged completed event is pruned"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM sync_events WHERE event_type = 'recent'"
        )
        .await,
        1,
        "the recent one is kept"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM sync_events WHERE event_type = 'old_running'"
        )
        .await,
        1,
        "a running event is never pruned by age"
    );
    assert_eq!(
        count(&db, "SELECT count(*) AS c FROM ingest_receipts").await,
        1,
        "the receipt inside the window survives"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// A zero retention day count is the documented way to keep a ledger forever, so it must delete
/// nothing rather than reading as "older than now".
#[tokio::test]
#[serial]
async fn zero_retention_days_prunes_nothing() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    let service = seed_service(&db, "rshiny", "1 minute", false).await;
    exec(
        &db,
        &format!(
            "INSERT INTO sync_events (id, service_id, event_type, status, started_at) \
             VALUES (gen_random_uuid(), '{service}', 'ancient', 'completed', NOW() - INTERVAL '900 days')"
        ),
    )
    .await;

    let mut config = crate::common::test_config();
    config.sync_event_retention_days = 0;
    config.ingest_receipt_retention_days = 0;

    assert_eq!(
        run_job(&db, Arc::new(SyncLedgerRetention::from_config(&config))).await,
        0
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM sync_events WHERE event_type = 'ancient'"
        )
        .await,
        1,
        "retention off keeps everything"
    );

    crate::common::cleanup_test_db(&db).await;
}
