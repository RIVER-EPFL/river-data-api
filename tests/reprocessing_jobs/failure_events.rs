//! A live watcher learns of a run's failure from the event stream, not only from a poll of the row:
//! a retryable failure announces `retrying` and an exhausted one announces `failed` carrying the
//! error, the way a success announces `completed`.
//!
//! Run: cargo test --test reprocessing_jobs failure_events -- --test-threads=1

use std::time::Duration;

use river_db::common::AppEvent;
use river_db::routes::private::reprocessing_jobs::lifecycle::RetryPolicy;
use river_db::routes::private::reprocessing_jobs::worker;
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
    let wid = worker::worker_id();
    let job_id = worker::enqueue(&db, "test_retry", None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .expect("a fresh enqueue inserts a row");

    while worker::run_one_with_policy(&db, &events, &registry, &wid, IMMEDIATE)
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
    let wid = worker::worker_id();
    let job_id = worker::enqueue(
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

    worker::run_one_with_policy(&db, &events, &registry, &wid, IMMEDIATE)
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
