//! The weekly full re-assert is a setting on the service, so two live services of one type can
//! disagree about it.
//!
//! Run: cargo test --test reprocessing_jobs -- sync_full_reassert --test-threads=1

use river_db::routes::private::reprocessing_jobs::{job, worker};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

fn events() -> river_db::common::EventSender {
    tokio::sync::broadcast::channel::<river_db::common::AppEvent>(16).0
}

async fn live_service(db: &DatabaseConnection, instance: &str, full_reassert: bool) -> Uuid {
    let id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO sync_services \
                 (id, service_type, instance_id, status, paused, full_reassert_enabled, \
                  last_heartbeat, created_at, updated_at) \
             VALUES ('{id}', 'rshiny', '{instance}', 'idle', false, {full_reassert}, NOW(), NOW(), NOW())"
        ),
    )
    .await;
    id
}

#[tokio::test]
#[serial]
async fn only_the_enabled_service_is_queued() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let enabled = live_service(&db, "cnet-1", true).await;
    let _disabled = live_service(&db, "metalp-1", false).await;

    let job_id = worker::enqueue(&db, "sync_full_reassert", None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .expect("the re-assert is enqueued");
    let registry = {
        let mut registry = job::build_registry();
        job::register_scheduled_services(&mut registry, &crate::common::test_config());
        registry
    };
    assert!(
        worker::run_one(&db, &events(), &registry, &worker::worker_id())
            .await
            .unwrap(),
        "the worker claims the re-assert"
    );

    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT service_id FROM sync_commands WHERE command = 'trigger_full_sync'".to_string(),
        ))
        .await
        .unwrap();
    let queued: Vec<Uuid> = rows
        .iter()
        .map(|r| r.try_get::<Uuid>("", "service_id").unwrap())
        .collect();
    assert_eq!(
        queued,
        vec![enabled],
        "exactly the service with the flag on is queued"
    );

    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT status, detail FROM reprocessing_jobs WHERE id = '{job_id}'"),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<String>("", "status").unwrap(), "completed");
    let detail: serde_json::Value = row.try_get("", "detail").unwrap();
    assert_eq!(detail["counts"]["commands_queued"], serde_json::json!(1));
    assert_eq!(
        detail["scope"]["service_ids"],
        serde_json::json!([enabled.to_string()]),
        "the run names what it queued: {detail}"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// A paused service is left alone whatever its flag says: pausing stops the schedule, and the
/// re-assert is a scheduled pass.
#[tokio::test]
#[serial]
async fn a_paused_service_is_not_queued() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let id = live_service(&db, "cnet-paused", true).await;
    crate::common::exec(
        &db,
        &format!("UPDATE sync_services SET paused = true WHERE id = '{id}'"),
    )
    .await;

    worker::enqueue(&db, "sync_full_reassert", None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .expect("the re-assert is enqueued");
    let registry = {
        let mut registry = job::build_registry();
        job::register_scheduled_services(&mut registry, &crate::common::test_config());
        registry
    };
    assert!(
        worker::run_one(&db, &events(), &registry, &worker::worker_id())
            .await
            .unwrap()
    );

    assert_eq!(
        crate::common::e2e::count(
            &db,
            "SELECT COUNT(*) FROM sync_commands WHERE command = 'trigger_full_sync'",
        )
        .await,
        0,
        "a paused service is queued nothing"
    );

    crate::common::cleanup_test_db(&db).await;
}
