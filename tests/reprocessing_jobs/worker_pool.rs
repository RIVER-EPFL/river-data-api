//! The claim-based multi-replica worker pool: a worker claims a due (or reapable) job with
//! `SELECT … FOR UPDATE SKIP LOCKED`, runs the registered `Job`, and commits ownership-guarded.
//! Covers claim→complete, the reaper reclaiming an expired lease, SKIP LOCKED exclusivity under
//! concurrent claims, and failure recording / lease release.
//!
//! Run: cargo test --test reprocessing_jobs -- --test-threads=1

use async_trait::async_trait;
use river_db::common::AppEvent;
use river_db::routes::private::reprocessing_jobs::service as jobs;
use river_db::routes::private::reprocessing_jobs::service::JobContext;
use river_db::routes::private::reprocessing_jobs::service::{Job, JobRegistry};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;

/// Always completes, returning a fixed count and tallying how many times it ran.
struct CompletingJob {
    name: &'static str,
    count: i64,
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl Job for CompletingJob {
    fn name(&self) -> &'static str {
        self.name
    }
    async fn run(&self, _ctx: JobContext) -> Result<i64, sea_orm::DbErr> {
        self.runs.fetch_add(1, Ordering::Relaxed);
        Ok(self.count)
    }
}

/// Panics instead of returning, stands in for a handler bug.
struct PanickingJob;

#[async_trait]
impl Job for PanickingJob {
    fn name(&self) -> &'static str {
        "test_panic"
    }
    async fn run(&self, _ctx: JobContext) -> Result<i64, sea_orm::DbErr> {
        panic!("handler exploded");
    }
}

/// Always fails.
struct FailingJob;

#[async_trait]
impl Job for FailingJob {
    fn name(&self) -> &'static str {
        "test_fail"
    }
    async fn run(&self, _ctx: JobContext) -> Result<i64, sea_orm::DbErr> {
        Err(sea_orm::DbErr::Custom("boom".into()))
    }
}

fn retry_budget(max_retries: u32) -> jobs::RetryPolicy {
    jobs::RetryPolicy {
        max_retries,
        backoff_base: std::time::Duration::from_secs(60),
    }
}

fn events() -> river_db::common::EventSender {
    tokio::sync::broadcast::channel::<AppEvent>(16).0
}

struct JobRow {
    status: String,
    readings_updated: Option<i32>,
    retry_count: i32,
    error_message: Option<String>,
    owner_is_null: bool,
    completed: bool,
}

async fn job_row(db: &DatabaseConnection, id: Uuid) -> JobRow {
    let r = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT status, readings_updated, retry_count, error_message, \
                        owner IS NULL AS owner_null, completed_at IS NOT NULL AS done \
                 FROM reprocessing_jobs WHERE id = '{id}'"
            ),
        ))
        .await
        .unwrap()
        .unwrap();
    JobRow {
        status: r.try_get("", "status").unwrap(),
        readings_updated: r.try_get("", "readings_updated").unwrap(),
        retry_count: r.try_get("", "retry_count").unwrap(),
        error_message: r.try_get("", "error_message").unwrap(),
        owner_is_null: r.try_get("", "owner_null").unwrap(),
        completed: r.try_get("", "done").unwrap(),
    }
}

#[tokio::test]
#[serial]
async fn claims_and_runs_to_completion() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let ev = events();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = JobRegistry::new();
    reg.register(Arc::new(CompletingJob {
        name: "test_complete",
        count: 7,
        runs: runs.clone(),
    }));
    let wid = jobs::worker_id();

    let id = jobs::enqueue(
        &db,
        "test_complete",
        None,
        None,
        &serde_json::json!({}),
        None,
    )
    .await
    .unwrap()
    .expect("a fresh enqueue inserts a row");

    assert!(jobs::run_one(&db, &ev, &reg, &wid).await.unwrap());
    assert_eq!(runs.load(Ordering::Relaxed), 1);

    let row = job_row(&db, id).await;
    assert_eq!(row.status, "completed");
    assert_eq!(row.readings_updated, Some(7));
    assert!(row.owner_is_null, "lease cleared on completion");
    assert!(row.completed);

    assert!(
        !jobs::run_one(&db, &ev, &reg, &wid).await.unwrap(),
        "queue is now empty"
    );
}

/// Scenario: a job left `running` by the pre-worker-pool `tokio::spawn` path, whose pod died. It
/// carries no lease at all, because leases did not exist when it was written.
///
/// Expected behaviour: a claimed row always carries a lease, so a `running` row with none is
/// exactly the orphan shape, and the reaper takes it.
#[tokio::test]
#[serial]
async fn reaper_reclaims_a_running_row_with_no_lease() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let ev = events();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = JobRegistry::new();
    reg.register(Arc::new(CompletingJob {
        name: "test_complete",
        count: 1,
        runs: runs.clone(),
    }));
    let wid = jobs::worker_id();

    let id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs \
                (id, trigger_type, status, category, owner, lease_epoch, lease_expires_at) \
             VALUES ('{id}', 'test_complete', 'running', 'operator', NULL, 0, NULL)"
        ),
    )
    .await;

    assert!(
        jobs::run_one_with_policy(&db, &ev, &reg, &wid, retry_budget(3))
            .await
            .unwrap(),
        "a running row with no lease is unreachable by nothing else, so the reaper must take it"
    );
    assert_eq!(runs.load(Ordering::Relaxed), 1);
    let row = job_row(&db, id).await;
    assert_eq!(row.status, "completed");
    assert!(row.owner_is_null);
}

#[tokio::test]
#[serial]
async fn reaper_reclaims_expired_lease() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let ev = events();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = JobRegistry::new();
    reg.register(Arc::new(CompletingJob {
        name: "test_complete",
        count: 3,
        runs: runs.clone(),
    }));
    let wid = jobs::worker_id();

    // A row stranded 'running' by a dead worker, lease long expired.
    let id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs \
                (id, trigger_type, status, category, owner, lease_epoch, lease_expires_at) \
             VALUES ('{id}', 'test_complete', 'running', 'operator', 'dead-worker', 5, \
                     now() - interval '5 minutes')"
        ),
    )
    .await;

    assert!(
        jobs::run_one_with_policy(&db, &ev, &reg, &wid, retry_budget(3))
            .await
            .unwrap(),
        "reaper should reclaim and run the expired-lease job"
    );
    assert_eq!(runs.load(Ordering::Relaxed), 1);
    let row = job_row(&db, id).await;
    assert_eq!(row.status, "completed");
    assert!(row.owner_is_null);
    assert_eq!(row.retry_count, 1, "the orphaned run spent an attempt");
}

/// Scenario: a job whose run never returns (the database or the pod dies under it) is orphaned
/// again and again, and has already spent its whole retry budget.
///
/// Expected behaviour: the reaper fails the row instead of running it once more.
#[tokio::test]
#[serial]
async fn reaper_fails_an_orphan_that_has_spent_its_retries() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let ev = events();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = JobRegistry::new();
    reg.register(Arc::new(CompletingJob {
        name: "test_complete",
        count: 1,
        runs: runs.clone(),
    }));
    let wid = jobs::worker_id();

    let id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs \
                (id, trigger_type, status, category, owner, lease_epoch, lease_expires_at, \
                 retry_count) \
             VALUES ('{id}', 'test_complete', 'running', 'operator', 'dead-worker', 9, \
                     now() - interval '5 minutes', 3)"
        ),
    )
    .await;

    assert!(
        jobs::run_one_with_policy(&db, &ev, &reg, &wid, retry_budget(3))
            .await
            .unwrap()
    );
    assert_eq!(
        runs.load(Ordering::Relaxed),
        0,
        "an exhausted orphan is not run"
    );
    let row = job_row(&db, id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.retry_count, 4);
    assert!(row.owner_is_null);
    assert!(
        row.error_message
            .as_deref()
            .is_some_and(|m| m.contains("lease expired")),
        "{:?}",
        row.error_message
    );
}

#[tokio::test]
#[serial]
async fn skip_locked_gives_exclusive_claim() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let ev = events();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = JobRegistry::new();
    reg.register(Arc::new(CompletingJob {
        name: "test_complete",
        count: 1,
        runs: runs.clone(),
    }));

    let id = jobs::enqueue(
        &db,
        "test_complete",
        None,
        None,
        &serde_json::json!({}),
        None,
    )
    .await
    .unwrap()
    .unwrap();

    let w1 = jobs::worker_id();
    let w2 = jobs::worker_id();
    let (r1, r2) = tokio::join!(
        jobs::run_one(&db, &ev, &reg, &w1),
        jobs::run_one(&db, &ev, &reg, &w2),
    );
    let (r1, r2) = (r1.unwrap(), r2.unwrap());
    assert!(
        r1 ^ r2,
        "exactly one worker may claim the single job (got {r1},{r2})"
    );
    assert_eq!(runs.load(Ordering::Relaxed), 1, "the job runs exactly once");
    assert_eq!(job_row(&db, id).await.status, "completed");
}

#[tokio::test]
#[serial]
async fn failure_records_error_and_releases_lease() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let ev = events();
    let mut reg = JobRegistry::new();
    reg.register(Arc::new(FailingJob));
    let wid = jobs::worker_id();

    let id = jobs::enqueue(&db, "test_fail", None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .unwrap();

    assert!(jobs::run_one(&db, &ev, &reg, &wid).await.unwrap());
    let row = job_row(&db, id).await;
    // `run_one` runs under the process-wide policy, which no test sets: no retries, so the failure
    // is terminal. The retry arm is `retry_backoff.rs`.
    assert!(row.error_message.unwrap_or_default().contains("boom"));
    assert!(row.owner_is_null, "lease released on failure");
    assert_eq!(row.retry_count, 1);
    assert_eq!(row.status, "failed");
}

#[tokio::test]
#[serial]
async fn handler_panic_fails_job_and_worker_survives() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let ev = events();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = JobRegistry::new();
    reg.register(Arc::new(PanickingJob));
    reg.register(Arc::new(CompletingJob {
        name: "test_complete",
        count: 9,
        runs: runs.clone(),
    }));
    let wid = jobs::worker_id();

    let panic_id = jobs::enqueue(&db, "test_panic", None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .unwrap();

    // The panic is caught inside the worker: `run_one` returns normally (no unwind through it), so a
    // panicking handler can't take down the replica's only worker task.
    assert!(
        jobs::run_one(&db, &ev, &reg, &wid).await.unwrap(),
        "the panicking job is claimed and handled without unwinding the worker"
    );
    let row = job_row(&db, panic_id).await;
    assert_eq!(
        row.status, "failed",
        "a panic terminalizes the job, not the worker"
    );
    assert!(
        row.error_message
            .unwrap_or_default()
            .to_lowercase()
            .contains("panic"),
        "the panic is recorded as the job error"
    );
    assert!(row.owner_is_null, "lease released after a panic");

    // The same worker claims and runs the next job, proof the loop wasn't killed.
    let ok_id = jobs::enqueue(
        &db,
        "test_complete",
        None,
        None,
        &serde_json::json!({}),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(jobs::run_one(&db, &ev, &reg, &wid).await.unwrap());
    assert_eq!(
        runs.load(Ordering::Relaxed),
        1,
        "worker still processes work after a panic"
    );
    assert_eq!(job_row(&db, ok_id).await.status, "completed");
}
