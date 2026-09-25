//! A live watcher learns of a run's failure from the event stream, not only from a poll of the row:
//! a retryable failure announces `retrying` and an exhausted one announces `failed` carrying the
//! error, the way a success announces `completed`. A progress announcement carries what the row
//! stored, so the stream and a reload agree.
//!
//! Run: cargo test --test reprocessing_jobs failure_events -- --test-threads=1

use std::time::Duration;

use river_db::common::AppEvent;
use river_db::routes::private::reprocessing_jobs::service as jobs;
use river_db::routes::private::reprocessing_jobs::service::RetryPolicy;
use sea_orm::DbErr;
use serial_test::serial;
use tokio::sync::broadcast::error::TryRecvError;

use crate::common::jobs::{ClosureJob, registry_of};

const IMMEDIATE: RetryPolicy = RetryPolicy {
    max_retries: 1,
    backoff_base: Duration::ZERO,
};

/// Every job event on the receiver, drained without blocking.
fn drain(rx: &mut tokio::sync::broadcast::Receiver<AppEvent>) -> Vec<AppEvent> {
    let mut seen = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(event) => seen.push(event),
            Err(TryRecvError::Empty | TryRecvError::Closed) => return seen,
            Err(TryRecvError::Lagged(_)) => {}
        }
    }
}

#[tokio::test]
#[serial]
async fn failing_run_announces_retrying_then_failed() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let job = ClosureJob::new("test_retry", |_ctx| async {
        Err::<i64, _>(DbErr::Custom("boom".into()))
    });
    let registry = registry_of(job);
    let events = tokio::sync::broadcast::channel::<AppEvent>(64).0;
    let mut rx = events.subscribe();
    let wid = jobs::worker_id();
    let job_id = jobs::enqueue(&db, "test_retry", None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .expect("a fresh enqueue inserts a row");

    while jobs::run_one_with_policy(&db, &events, &registry, &wid, IMMEDIATE)
        .await
        .unwrap()
    {}

    let seen = drain(&mut rx);
    let retrying = seen.iter().find(|e| {
        matches!(e, AppEvent::JobProgress { job_id: id, status, .. }
            if *id == job_id && status == "retrying")
    });
    assert!(
        retrying.is_some(),
        "the rescheduled attempt is announced as retrying, saw {seen:?}"
    );

    let failed = seen
        .iter()
        .find_map(|e| match e {
            AppEvent::JobCompleted {
                job_id: id,
                status,
                error_message,
                ..
            } if *id == job_id && status == "failed" => Some(error_message.clone()),
            _ => None,
        })
        .expect("the exhausted job is announced as failed");
    assert!(
        failed.is_some_and(|m| m.contains("boom")),
        "the failure announcement carries the error message"
    );
}

#[tokio::test]
#[serial]
async fn unregistered_trigger_type_announces_failed() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let registry = registry_of(ClosureJob::new("test_retry", |_ctx| async { Ok(0) }));
    let events = tokio::sync::broadcast::channel::<AppEvent>(64).0;
    let mut rx = events.subscribe();
    let wid = jobs::worker_id();
    let job_id = jobs::enqueue(
        &db,
        "test_unknown",
        None,
        None,
        &serde_json::json!({}),
        None,
    )
    .await
    .unwrap()
    .expect("a fresh enqueue inserts a row");

    jobs::run_one_with_policy(&db, &events, &registry, &wid, IMMEDIATE)
        .await
        .unwrap();

    let seen = drain(&mut rx);
    assert!(
        seen.iter().any(
            |e| matches!(e, AppEvent::JobCompleted { job_id: id, status, .. }
            if *id == job_id && status == "failed")
        ),
        "a job with no handler is announced failed, saw {seen:?}"
    );
}

/// The progress events one job announced, as (progress, total).
fn progress_of(seen: &[AppEvent], job_id: uuid::Uuid) -> Vec<(Option<i32>, Option<i32>)> {
    seen.iter()
        .filter_map(|e| match e {
            AppEvent::JobProgress {
                job_id: id,
                status,
                progress,
                total,
            } if *id == job_id && status == "running" => Some((*progress, *total)),
            _ => None,
        })
        .collect()
}

/// The stored (progress, total) of one job.
async fn stored_progress(
    db: &sea_orm::DatabaseConnection,
    job_id: uuid::Uuid,
) -> (Option<i32>, Option<i32>) {
    use sea_orm::EntityTrait;
    let row = river_db::routes::private::reprocessing_jobs::models::job::Entity::find_by_id(job_id)
        .one(db)
        .await
        .unwrap()
        .expect("the job row");
    (row.progress, row.total)
}

/// Scenario: a run sets its total with its first count, then reports counts alone.
/// Expected behaviour: the stored row and every announcement keep the total.
#[tokio::test]
#[serial]
async fn test_count_only_progress_keeps_the_stored_total() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let job = ClosureJob::new("test_progress", |ctx| async move {
        ctx.set_progress(0, Some(22)).await;
        ctx.set_progress(10, None).await;
        Ok(0)
    });
    let registry = registry_of(job);
    let events = tokio::sync::broadcast::channel::<AppEvent>(64).0;
    let mut rx = events.subscribe();
    let job_id = jobs::enqueue(&db, "test_progress", None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .expect("a fresh enqueue inserts a row");
    jobs::run_one_with_policy(&db, &events, &registry, &jobs::worker_id(), IMMEDIATE)
        .await
        .unwrap();

    assert_eq!(
        progress_of(&drain(&mut rx), job_id),
        vec![(Some(0), Some(22)), (Some(10), Some(22))]
    );
    assert_eq!(stored_progress(&db, job_id).await, (Some(10), Some(22)));
}

/// Scenario: the database refuses one progress write.
/// Expected behaviour: nothing is announced for it, so the stream never runs ahead of the row.
#[tokio::test]
#[serial]
async fn test_a_progress_write_that_fails_is_not_announced() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    for sql in [
        "DROP TRIGGER IF EXISTS zz_progress_fault ON reprocessing_jobs",
        "CREATE OR REPLACE FUNCTION progress_fault() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             IF NEW.progress = 13 THEN RAISE EXCEPTION 'injected progress fault'; END IF;
             RETURN NEW;
         END $$",
        "CREATE TRIGGER zz_progress_fault BEFORE UPDATE ON reprocessing_jobs
         FOR EACH ROW EXECUTE FUNCTION progress_fault()",
    ] {
        crate::common::exec(&db, sql).await;
    }

    let job = ClosureJob::new("test_progress", |ctx| async move {
        ctx.set_progress(5, Some(20)).await;
        ctx.set_progress(13, None).await;
        Ok(0)
    });
    let registry = registry_of(job);
    let events = tokio::sync::broadcast::channel::<AppEvent>(64).0;
    let mut rx = events.subscribe();
    let job_id = jobs::enqueue(&db, "test_progress", None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .expect("a fresh enqueue inserts a row");
    jobs::run_one_with_policy(&db, &events, &registry, &jobs::worker_id(), IMMEDIATE)
        .await
        .unwrap();
    let stored = stored_progress(&db, job_id).await;
    for sql in [
        "DROP TRIGGER IF EXISTS zz_progress_fault ON reprocessing_jobs",
        "DROP FUNCTION IF EXISTS progress_fault()",
    ] {
        crate::common::exec(&db, sql).await;
    }

    assert_eq!(
        progress_of(&drain(&mut rx), job_id),
        vec![(Some(5), Some(20))]
    );
    assert_eq!(stored, (Some(5), Some(20)));
}
