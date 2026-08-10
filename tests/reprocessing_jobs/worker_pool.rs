//! The claim-based multi-replica worker pool: a worker claims a due (or reapable) job with
//! `SELECT … FOR UPDATE SKIP LOCKED`, runs the registered `Job`, and commits ownership-guarded.
//! Covers claim→complete, the reaper reclaiming an expired lease, SKIP LOCKED exclusivity under
//! concurrent claims, and failure recording / lease release.
//!
//! Run: cargo test --test reprocessing_jobs -- --test-threads=1

use async_trait::async_trait;
use river_db::common::AppEvent;
use river_db::routes::private::reprocessing_jobs::job::{Job, JobRegistry};
use river_db::routes::private::reprocessing_jobs::lifecycle::{self, JobContext};
use river_db::routes::private::reprocessing_jobs::worker;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
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

/// Panics instead of returning — stands in for a handler bug.
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
        .query_one(Statement::from_string(
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
    let wid = worker::worker_id();

    let id = worker::enqueue(&db, "test_complete", None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .expect("a fresh enqueue inserts a row");

    assert!(worker::run_one(&db, &ev, &reg, &wid).await.unwrap());
    assert_eq!(runs.load(Ordering::Relaxed), 1);

    let row = job_row(&db, id).await;
    assert_eq!(row.status, "completed");
    assert_eq!(row.readings_updated, Some(7));
    assert!(row.owner_is_null, "lease cleared on completion");
    assert!(row.completed);

    assert!(
        !worker::run_one(&db, &ev, &reg, &wid).await.unwrap(),
        "queue is now empty"
    );
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
    let wid = worker::worker_id();

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
        worker::run_one(&db, &ev, &reg, &wid).await.unwrap(),
        "reaper should reclaim and run the expired-lease job"
    );
    assert_eq!(runs.load(Ordering::Relaxed), 1);
    let row = job_row(&db, id).await;
    assert_eq!(row.status, "completed");
    assert!(row.owner_is_null);
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

    let id = worker::enqueue(&db, "test_complete", None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .unwrap();

    let w1 = worker::worker_id();
    let w2 = worker::worker_id();
    let (r1, r2) = tokio::join!(
        worker::run_one(&db, &ev, &reg, &w1),
        worker::run_one(&db, &ev, &reg, &w2),
    );
    let (r1, r2) = (r1.unwrap(), r2.unwrap());
    assert!(r1 ^ r2, "exactly one worker may claim the single job (got {r1},{r2})");
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
    let wid = worker::worker_id();

    let id = worker::enqueue(&db, "test_fail", None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .unwrap();

    assert!(worker::run_one(&db, &ev, &reg, &wid).await.unwrap());
    let row = job_row(&db, id).await;
    // Default policy is no-retries → 'failed'; under a retry policy the same failure reschedules to
    // 'pending'. Either way the error is recorded, the lease released, and the attempt counted — so
    // assert the policy-independent invariants.
    assert!(row.error_message.unwrap_or_default().contains("boom"));
    assert!(row.owner_is_null, "lease released on failure");
    assert_eq!(row.retry_count, 1);
    assert!(matches!(row.status.as_str(), "failed" | "pending"));
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
    let wid = worker::worker_id();

    let panic_id = worker::enqueue(&db, "test_panic", None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .unwrap();

    // The panic is caught inside the worker: `run_one` returns normally (no unwind through it), so a
    // panicking handler can't take down the replica's only worker task.
    assert!(
        worker::run_one(&db, &ev, &reg, &wid).await.unwrap(),
        "the panicking job is claimed and handled without unwinding the worker"
    );
    let row = job_row(&db, panic_id).await;
    assert!(
        matches!(row.status.as_str(), "failed" | "pending"),
        "a panic terminalizes the job (got {}), not the worker",
        row.status
    );
    assert!(
        row.error_message.unwrap_or_default().to_lowercase().contains("panic"),
        "the panic is recorded as the job error"
    );
    assert!(row.owner_is_null, "lease released after a panic");

    // The same worker claims and runs the next job — proof the loop wasn't killed.
    let ok_id = worker::enqueue(&db, "test_complete", None, None, &serde_json::json!({}), None)
        .await
        .unwrap()
        .unwrap();
    assert!(worker::run_one(&db, &ev, &reg, &wid).await.unwrap());
    assert_eq!(runs.load(Ordering::Relaxed), 1, "worker still processes work after a panic");
    assert_eq!(job_row(&db, ok_id).await.status, "completed");
}

#[tokio::test]
#[serial]
async fn startup_reaps_own_leaseless_orphans_only() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let own = lifecycle::process_owner();

    // This replica's crashed in-process job: running, our owner, no lease → the reaper can't see it
    // (it keys on an expired lease), so the startup sweep must reclaim it.
    let orphan = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status, category, owner) \
             VALUES ('{orphan}', 'x_orphan', 'running', 'operator', '{own}')"
        ),
    )
    .await;

    // A peer replica's leaseless orphan (different owner) — left for that pod's own boot.
    let peer = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status, category, owner) \
             VALUES ('{peer}', 'x_peer', 'running', 'operator', 'other-pod')"
        ),
    )
    .await;

    // A live worker-pool job: running, worker owner, valid lease — must not be touched.
    let leased = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs \
                (id, trigger_type, status, category, owner, lease_expires_at) \
             VALUES ('{leased}', 'x_leased', 'running', 'operator', 'worker-live', \
                     now() + interval '5 minutes')"
        ),
    )
    .await;

    let reclaimed = lifecycle::reconcile_orphaned_inline_jobs(&db).await.unwrap();
    assert_eq!(reclaimed, 1, "only this replica's leaseless orphan is reaped");

    let orphan_row = job_row(&db, orphan).await;
    assert_eq!(orphan_row.status, "failed", "the own orphan is failed");
    assert!(orphan_row.owner_is_null, "the reaped orphan's owner is cleared");
    assert!(orphan_row.completed, "the reaped orphan gets a completed_at");
    assert_eq!(
        job_row(&db, peer).await.status,
        "running",
        "a peer's orphan is left for its own boot"
    );
    assert_eq!(
        job_row(&db, leased).await.status,
        "running",
        "a live leased worker job is untouched"
    );
}

#[tokio::test]
#[serial]
async fn worker_runs_registered_reprocess_job() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let ev = events();
    let registry = river_db::routes::private::reprocessing_jobs::job::build_registry();
    let wid = worker::worker_id();

    let sensor_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, is_active) \
             VALUES ('{sensor_id}', 'Worker-Probe', true)"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensor_calibrations (id, sensor_id, slope, intercept, valid_from, notes) \
             VALUES ('{}', '{sensor_id}', 1.0, 0.0, '2000-01-01T00:00:00Z', 'identity')",
            Uuid::new_v4()
        ),
    )
    .await;

    let id = worker::enqueue(
        &db,
        "manual_reprocess",
        Some(sensor_id),
        None,
        &serde_json::json!({ "sensor_id": sensor_id }),
        None,
    )
    .await
    .unwrap()
    .unwrap();

    worker::drain(&db, &ev, &registry, &wid).await.unwrap();

    let row = job_row(&db, id).await;
    assert_eq!(row.status, "completed", "the registered ReprocessSensor job runs end to end");
    let detail: serde_json::Value = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT detail FROM reprocessing_jobs WHERE id = '{id}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "detail")
        .unwrap();
    assert_eq!(detail["scope"]["sensor_id"], serde_json::json!(sensor_id));
}
