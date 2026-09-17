//! Worker-pool retry: a failed run is rescheduled `queued` with a bumped `retry_count` and retried
//! by the next claim until it succeeds, and a persistently failing job is `failed` once the retry
//! budget is spent. Drives `run_one_with_policy` with a zero backoff so every retry is claimable
//! at once and the test is deterministic.
//!
//! Run: cargo test --test reprocessing_jobs -- --test-threads=1

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use river_db::routes::private::reprocessing_jobs::service as jobs;
use river_db::routes::private::reprocessing_jobs::service::RetryPolicy;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, Statement};
use serial_test::serial;

use crate::common::jobs::{ClosureJob, registry_of};

fn events() -> river_db::common::EventSender {
    tokio::sync::broadcast::channel::<river_db::common::AppEvent>(16).0
}

const IMMEDIATE: RetryPolicy = RetryPolicy {
    max_retries: 2,
    backoff_base: Duration::ZERO,
};

async fn job_row(db: &DatabaseConnection, job_id: uuid::Uuid) -> (String, Option<i32>, i32) {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT status, readings_updated, retry_count FROM reprocessing_jobs WHERE id = '{job_id}'"
            ),
        ))
        .await
        .unwrap()
        .expect("the enqueued job row");
    (
        row.try_get("", "status").unwrap(),
        row.try_get("", "readings_updated").unwrap(),
        row.try_get("", "retry_count").unwrap(),
    )
}

/// Enqueue one `test_retry` job and run worker cycles until nothing is claimable.
async fn enqueue_and_drain(
    db: &DatabaseConnection,
    job: ClosureJob,
    policy: RetryPolicy,
) -> uuid::Uuid {
    let registry = registry_of(job);
    let ev = events();
    let wid = jobs::worker_id();
    let job_id = jobs::enqueue(db, "test_retry", None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .expect("a fresh enqueue inserts a row");
    while jobs::run_one_with_policy(db, &ev, &registry, &wid, policy)
        .await
        .unwrap()
    {}
    job_id
}

#[tokio::test]
#[serial]
async fn job_retries_then_succeeds() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let attempts = Arc::new(AtomicU32::new(0));
    let a = attempts.clone();
    // Fail the first two attempts, succeed on the third.
    let job = ClosureJob::new("test_retry", move |_ctx| {
        let a = a.clone();
        async move {
            if a.fetch_add(1, Ordering::SeqCst) < 2 {
                Err(DbErr::Custom("transient boom".into()))
            } else {
                Ok(7)
            }
        }
    });

    let job_id = enqueue_and_drain(&db, job, IMMEDIATE).await;

    let (status, readings_updated, retry_count) = job_row(&db, job_id).await;
    assert_eq!(status, "completed", "succeeds after retrying");
    assert_eq!(
        readings_updated,
        Some(7),
        "completed job records the work's count"
    );
    assert_eq!(retry_count, 2, "two failed attempts before success");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "work invoked three times total"
    );
}

#[tokio::test]
#[serial]
async fn job_exhausts_retries_then_fails() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let attempts = Arc::new(AtomicU32::new(0));
    let a = attempts.clone();
    let job = ClosureJob::new("test_retry", move |_ctx| {
        let a = a.clone();
        async move {
            a.fetch_add(1, Ordering::SeqCst);
            Err::<i64, _>(DbErr::Custom("always boom".into()))
        }
    });

    let job_id = enqueue_and_drain(&db, job, IMMEDIATE).await;

    let (status, _readings_updated, retry_count) = job_row(&db, job_id).await;
    assert_eq!(status, "failed", "fails once retries are exhausted");
    assert_eq!(retry_count, 3, "every failed attempt is counted");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "initial attempt + two retries"
    );
}

#[tokio::test]
#[serial]
async fn failed_run_is_rescheduled_queued_with_backoff() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let job = ClosureJob::new("test_retry", |_ctx| async {
        Err::<i64, _>(DbErr::Custom("boom".into()))
    });
    let policy = RetryPolicy {
        max_retries: 1,
        backoff_base: Duration::from_secs(60),
    };
    let job_id = enqueue_and_drain(&db, job, policy).await;

    // The first failure reschedules the row; a 60s backoff keeps the second cycle from claiming it,
    // so the retry outlives this process instead of a timer in it.
    let (status, _readings_updated, retry_count) = job_row(&db, job_id).await;
    assert_eq!(
        status, "queued",
        "a retryable failure is queued, not failed"
    );
    assert_eq!(retry_count, 1);
    let due_later: bool = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT next_attempt_at > now() + interval '30 seconds' AS v \
                 FROM reprocessing_jobs WHERE id = '{job_id}'"
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "v")
        .unwrap();
    assert!(due_later, "the backoff is persisted as next_attempt_at");
}

/// Scenario: a job fails once and succeeds on its retry.
/// Expected behaviour: the retry's timeline continues after the first attempt's lines, so both
/// attempts' opening and closing lines are kept.
#[tokio::test]
#[serial]
async fn retried_run_keeps_every_attempts_timeline() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let attempts = Arc::new(AtomicU32::new(0));
    let a = attempts.clone();
    let job = ClosureJob::new("test_retry", move |_ctx| {
        let a = a.clone();
        async move {
            if a.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err::<i64, _>(DbErr::Custom("first attempt".into()));
            }
            Ok(1)
        }
    });
    let job_id = enqueue_and_drain(&db, job, IMMEDIATE).await;

    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT seq, context->>'attempt' AS attempt FROM reprocessing_job_logs \
                 WHERE job_id = '{job_id}' ORDER BY seq"
            ),
        ))
        .await
        .unwrap();
    let seqs: Vec<i64> = rows.iter().map(|r| r.try_get("", "seq").unwrap()).collect();
    let openings: Vec<String> = rows
        .iter()
        .filter_map(|r| r.try_get::<Option<String>>("", "attempt").unwrap())
        .collect();
    assert_eq!(seqs, vec![0, 1, 2, 3], "two lines per attempt");
    assert_eq!(openings, vec!["1", "2"]);
}
