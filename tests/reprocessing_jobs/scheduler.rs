//! The DB-backed recurring-Service scheduler: due `schedules` rows are claimed and enqueued once per
//! scheduled slot across the fleet, and the cadence grid advances drift-free off the scheduled time.
//!
//! Covers: seeding from `default_schedules`; a due row enqueues exactly one job and advances
//! `next_run_at`; a second tick in the same slot does NOT double-enqueue (dedupe_key); a not-yet-due
//! row is left alone; and overlap `skip_if_running` suppresses the enqueue while a run is in flight.
//!
//! Run: cargo test --test reprocessing_jobs -- --test-threads=1

use std::sync::Arc;

use async_trait::async_trait;
use river_db::routes::private::reprocessing_jobs::job::{Job, JobRegistry};
use river_db::routes::private::reprocessing_jobs::lifecycle::JobContext;
use river_db::routes::private::reprocessing_jobs::schedule::Schedule;
use river_db::routes::private::reprocessing_jobs::scheduler;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

/// A schedulable test Service: completes immediately and carries a default cadence so it seeds.
struct TestService {
    name: &'static str,
    interval_secs: i64,
}

#[async_trait]
impl Job for TestService {
    fn name(&self) -> &'static str {
        self.name
    }
    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(self.interval_secs))
    }
    async fn run(&self, _ctx: JobContext) -> Result<i64, sea_orm::DbErr> {
        Ok(0)
    }
}

fn registry_with(name: &'static str, interval_secs: i64) -> Arc<JobRegistry> {
    let mut reg = JobRegistry::new();
    reg.register(Arc::new(TestService { name, interval_secs }));
    Arc::new(reg)
}

async fn count_queued(db: &DatabaseConnection, job_name: &str) -> i64 {
    db.query_one(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT count(*) AS n FROM reprocessing_jobs WHERE trigger_type = $1 AND status = 'queued'",
        [job_name.into()],
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

/// Force a schedule row due now (and pin its `next_run_at` to a known scheduled time so the dedupe
/// key is deterministic across ticks).
async fn force_due(db: &DatabaseConnection, job_name: &str) {
    crate::common::exec(
        db,
        &format!(
            "UPDATE schedules SET next_run_at = now() - interval '1 second' WHERE job_name = '{job_name}'"
        ),
    )
    .await;
}

async fn next_run_at(db: &DatabaseConnection, job_name: &str) -> chrono::DateTime<chrono::Utc> {
    db.query_one(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT next_run_at FROM schedules WHERE job_name = $1",
        [job_name.into()],
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<chrono::DateTime<chrono::Utc>>("", "next_run_at")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn seed_inserts_one_row_per_service_idempotently() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let reg = registry_with("sched_seed_probe", 300);

    scheduler::seed_default_schedules(&db, &reg).await.unwrap();
    scheduler::seed_default_schedules(&db, &reg).await.unwrap();

    let n: i64 = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*) AS n FROM schedules WHERE job_name = 'sched_seed_probe'".to_string(),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "n")
        .unwrap();
    assert_eq!(n, 1, "seeding twice inserts exactly one row (ON CONFLICT DO NOTHING)");
}

#[tokio::test]
#[serial]
async fn due_service_enqueues_once_and_advances_next_run() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let reg = registry_with("sched_due_probe", 300);

    scheduler::seed_default_schedules(&db, &reg).await.unwrap();
    force_due(&db, "sched_due_probe").await;

    let before = next_run_at(&db, "sched_due_probe").await;
    let enqueued = scheduler::tick(&db, &reg).await.unwrap();
    assert_eq!(enqueued, 1, "the due service is enqueued exactly once");
    assert_eq!(count_queued(&db, "sched_due_probe").await, 1);

    let after = next_run_at(&db, "sched_due_probe").await;
    assert!(after > before, "next_run_at advanced past the scheduled slot");
    assert!(after > chrono::Utc::now(), "next_run_at is now in the future");
}

#[tokio::test]
#[serial]
async fn second_tick_same_slot_does_not_double_enqueue() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let reg = registry_with("sched_dedupe_probe", 300);

    scheduler::seed_default_schedules(&db, &reg).await.unwrap();

    // Pin the schedule to a fixed past scheduled time so BOTH ticks compute the same dedupe key
    // (job_name + scheduled epoch). The first advances next_run_at; we reset it back to the same slot
    // to emulate a second replica racing the identical slot.
    crate::common::exec(
        &db,
        "UPDATE schedules SET next_run_at = '2020-01-01T00:00:00Z' WHERE job_name = 'sched_dedupe_probe'",
    )
    .await;
    let enq1 = scheduler::tick(&db, &reg).await.unwrap();

    crate::common::exec(
        &db,
        "UPDATE schedules SET next_run_at = '2020-01-01T00:00:00Z' WHERE job_name = 'sched_dedupe_probe'",
    )
    .await;
    let enq2 = scheduler::tick(&db, &reg).await.unwrap();

    assert_eq!(enq1, 1, "first tick enqueues the slot");
    assert_eq!(enq2, 0, "re-running the same scheduled slot is deduped (no second job)");
    assert_eq!(
        count_queued(&db, "sched_dedupe_probe").await,
        1,
        "exactly one queued job exists for the slot"
    );
}

#[tokio::test]
#[serial]
async fn not_yet_due_service_is_left_alone() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let reg = registry_with("sched_future_probe", 300);

    scheduler::seed_default_schedules(&db, &reg).await.unwrap();
    // Seed sets next_run_at one interval out — already in the future, so nothing is due.
    let enqueued = scheduler::tick(&db, &reg).await.unwrap();
    assert_eq!(enqueued, 0, "a future schedule is not enqueued");
    assert_eq!(count_queued(&db, "sched_future_probe").await, 0);
}

#[tokio::test]
#[serial]
async fn skip_if_running_suppresses_enqueue_while_active() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let reg = registry_with("sched_overlap_probe", 300);

    scheduler::seed_default_schedules(&db, &reg).await.unwrap();
    // A prior run of this service is still in flight.
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status, category) \
             VALUES ('{}', 'sched_overlap_probe', 'running', 'maintenance')",
            uuid::Uuid::new_v4()
        ),
    )
    .await;
    force_due(&db, "sched_overlap_probe").await;

    let before = next_run_at(&db, "sched_overlap_probe").await;
    let enqueued = scheduler::tick(&db, &reg).await.unwrap();
    assert_eq!(enqueued, 0, "overlap=skip_if_running suppresses the enqueue while a run is active");
    assert_eq!(
        count_queued(&db, "sched_overlap_probe").await,
        0,
        "no new queued job while the prior run is in flight"
    );
    let after = next_run_at(&db, "sched_overlap_probe").await;
    assert!(after > before, "the cadence grid still advances so it never drifts");
}
