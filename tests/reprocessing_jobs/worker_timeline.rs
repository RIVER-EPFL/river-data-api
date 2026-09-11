//! Every run says what it started and how it ended, whatever the job body chooses to say. The
//! worker writes both lines, so a job that logs nothing still leaves a timeline rather than an
//! empty panel that reads as "nothing happened".
//!
//! Run: cargo test --test reprocessing_jobs worker_timeline -- --test-threads=1

use river_db::routes::private::reprocessing_jobs::service::Job;
use river_db::routes::private::reprocessing_jobs::service as jobs;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use crate::common::jobs::{ClosureJob, registry_of};

fn events() -> river_db::common::EventSender {
    tokio::sync::broadcast::channel::<river_db::common::AppEvent>(16).0
}

async fn run_job(db: &DatabaseConnection, job: ClosureJob) {
    let trigger_type = job.name();
    let registry = registry_of(job);
    jobs::enqueue(db, trigger_type, None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .expect("a fresh enqueue inserts a row");
    assert!(
        jobs::run_one(db, &events(), &registry, &jobs::worker_id())
            .await
            .unwrap(),
        "the worker claims the enqueued job"
    );
}

async fn timeline(db: &DatabaseConnection) -> Vec<(String, String, serde_json::Value)> {
    db.query_all_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT level, message, context FROM reprocessing_job_logs ORDER BY seq".to_owned(),
    ))
    .await
    .unwrap()
    .iter()
    .map(|r| {
        (
            r.try_get::<String>("", "level").unwrap(),
            r.try_get::<String>("", "message").unwrap(),
            r.try_get::<serde_json::Value>("", "context").unwrap(),
        )
    })
    .collect()
}

#[tokio::test]
#[serial]
async fn a_silent_job_still_leaves_its_opening_and_closing_lines() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    run_job(
        &db,
        ClosureJob::new("janitor_service", |_ctx| async { Ok(4) }),
    )
    .await;

    let lines = timeline(&db).await;
    assert_eq!(lines.len(), 2, "opening and closing: {lines:?}");
    assert_eq!(lines[0].0, "info");
    assert_eq!(lines[0].2["trigger_type"], "janitor_service");
    assert_eq!(lines[0].2["attempt"], 1);
    assert_eq!(lines[1].0, "info");
    assert_eq!(lines[1].2["status"], "completed");
    assert_eq!(lines[1].2["reported"], 4);
}

#[tokio::test]
#[serial]
async fn a_failed_run_closes_its_timeline_with_what_went_wrong() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    run_job(
        &db,
        ClosureJob::new("janitor_service", |_ctx| async {
            Err(sea_orm::DbErr::Custom("the slot moved".to_string()))
        }),
    )
    .await;

    let lines = timeline(&db).await;
    let last = lines
        .last()
        .expect("a failed run still closes its timeline");
    assert_eq!(last.0, "error", "{lines:?}");
    assert_eq!(last.2["status"], "failed");
    assert!(
        last.2["error"].as_str().unwrap().contains("the slot moved"),
        "the closing line carries the failure: {lines:?}"
    );
}
