//! DB-backed recurring-Service scheduler (ADR 0001, Wave 2).
//!
//! Every replica runs this tick loop, but each due Service fires exactly once per scheduled slot
//! across the whole fleet. Two layers guarantee that at 2-3 k8s replicas:
//!
//!   1. **Row claim.** A tick selects due `schedules` rows with `FOR UPDATE SKIP LOCKED` inside one
//!      transaction, advances each row's `next_run_at` off its *scheduled* time (drift-free, via
//!      [`schedule::next_run_after`]), and commits. A peer ticking concurrently skips the locked row
//!      and sees the advanced `next_run_at`, so it won't re-pick the same slot.
//!   2. **Enqueue dedupe.** The job is enqueued with `dedupe_key = "{job_name}:{scheduled_epoch}"`;
//!      [`worker::enqueue`] is `ON CONFLICT (dedupe_key) DO NOTHING`, so even if two replicas raced
//!      the same slot (clock skew, a missed lock), only one `queued` job is ever created.
//!
//! Overlap policy `SkipIfRunning` (the default) additionally skips the enqueue when a non-terminal
//! job of that `job_name` already exists, so a slow run can't stack up behind itself. The schedule's
//! `next_run_at` is still advanced regardless, so the cadence grid never drifts.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement, TransactionTrait};

use super::job::JobRegistry;
use super::schedule::{self, CatchupPolicy, OverlapPolicy};
use super::worker;

/// How often each replica scans `schedules` for due rows. Well below the shortest Service cadence
/// (the notification dispatcher's ~60s) so a due slot fires within a few seconds.
pub const TICK_SECONDS: u64 = 5;

/// Seed a `schedules` row for every recurring Service the registry knows about. Idempotent
/// (`ON CONFLICT (job_name) DO NOTHING`), so an operator's later cadence edits survive a restart and
/// only a genuinely new Service inserts a row. `next_run_at` starts one interval out from now.
pub async fn seed_default_schedules(
    db: &DatabaseConnection,
    registry: &JobRegistry,
) -> Result<(), sea_orm::DbErr> {
    let now = Utc::now();
    for (job_name, sched) in registry.default_schedules() {
        let interval_seconds = sched.interval.num_seconds().max(1);
        let next_run_at = now + sched.interval;
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO schedules \
                 (job_name, enabled, next_run_at, interval_seconds, overlap_policy, catchup_policy) \
             VALUES ($1, true, $2, $3, $4, $5) \
             ON CONFLICT (job_name) DO NOTHING",
            [
                job_name.into(),
                next_run_at.into(),
                interval_seconds.into(),
                sched.overlap.as_str().into(),
                sched.catchup.as_str().into(),
            ],
        ))
        .await?;
    }
    Ok(())
}

/// A due schedule row claimed for this tick.
struct DueSchedule {
    job_name: String,
    /// The slot time this firing represents, the `next_run_at` that just came due. The dedupe key
    /// is built from this so a re-pick of the same slot collapses to one job.
    scheduled_at: DateTime<Utc>,
    interval_seconds: i64,
    overlap: OverlapPolicy,
    catchup: CatchupPolicy,
    /// Whether this slot is a backlog slot rather than the cadence coming round: its scheduled time
    /// is a full interval or more behind now, which only happens after a gap in the scheduler.
    missed: bool,
    /// The operator-edited tunables snapshot, carried onto the enqueued job's params. Jobs read it
    /// from `ctx.params()["tunables"]`; it is fixed at enqueue time, so a mid-run edit to the
    /// schedule does NOT affect a job already queued/running (intentional, a run uses one snapshot).
    tunables: serde_json::Value,
}

/// One scheduler pass: claim due rows, advance their grid, and enqueue each (honoring overlap).
/// Returns how many Services were enqueued this tick. Separated from [`run`] so a test can drive a
/// single deterministic tick. Each due row is claimed and advanced in its own short transaction so a
/// slow enqueue can't hold a lock across the whole batch.
pub async fn tick(
    db: &DatabaseConnection,
    registry: &JobRegistry,
) -> Result<usize, sea_orm::DbErr> {
    let mut enqueued = 0usize;
    loop {
        let Some(due) = claim_one_due(db).await? else {
            break;
        };
        if enqueue_due(db, registry, &due).await? {
            enqueued += 1;
        }
    }
    Ok(enqueued)
}

/// Claim one due, enabled schedule row (`FOR UPDATE SKIP LOCKED`), advance its `next_run_at` off the
/// scheduled time (drift-free), and stamp `last_enqueued_at`, all in one transaction so a peer can't
/// re-pick the same slot. Returns the claimed slot (whose job the caller then enqueues) or `None`
/// when nothing is due.
async fn claim_one_due(db: &DatabaseConnection) -> Result<Option<DueSchedule>, sea_orm::DbErr> {
    let txn = db.begin().await?;
    let row = txn
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, job_name, next_run_at, interval_seconds, overlap_policy, catchup_policy, tunables \
             FROM schedules \
             WHERE enabled AND next_run_at IS NOT NULL AND next_run_at <= now() \
             ORDER BY next_run_at \
             FOR UPDATE SKIP LOCKED \
             LIMIT 1"
                .to_string(),
        ))
        .await?;

    let Some(row) = row else {
        txn.commit().await?;
        return Ok(None);
    };

    let id: uuid::Uuid = row.try_get("", "id")?;
    let job_name: String = row.try_get("", "job_name")?;
    let scheduled_at: DateTime<Utc> = row.try_get("", "next_run_at")?;
    let interval_seconds: i64 = row
        .try_get::<i64>("", "interval_seconds")
        .unwrap_or(0)
        .max(1);
    let overlap = OverlapPolicy::from_str_or_default(
        row.try_get::<Option<String>>("", "overlap_policy")?
            .as_deref(),
    );
    // `tunables` is `NOT NULL DEFAULT '{}'`; default to an empty object if a hand-edited row is null.
    let tunables: serde_json::Value = row
        .try_get::<Option<serde_json::Value>>("", "tunables")?
        .unwrap_or_else(|| serde_json::json!({}));

    let catchup = CatchupPolicy::from_str_or_default(
        row.try_get::<Option<String>>("", "catchup_policy")?
            .as_deref(),
    );

    // Advance the grid off the SCHEDULED time, not `now()`, so cadence never drifts by a run's own
    // latency and a downtime gap snaps forward to the next future slot (discarding the backlog).
    let now = Utc::now();
    let interval = chrono::Duration::seconds(interval_seconds);
    let missed = scheduled_at + interval <= now;
    let next = schedule::next_run_after(scheduled_at, interval, now);
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE schedules SET next_run_at = $1, last_enqueued_at = now() WHERE id = $2",
        [next.into(), id.into()],
    ))
    .await?;
    txn.commit().await?;

    Ok(Some(DueSchedule {
        job_name,
        scheduled_at,
        interval_seconds,
        overlap,
        catchup,
        missed,
        tunables,
    }))
}

/// Enqueue the job for a claimed slot, honoring its catchup and overlap policies. Returns whether a
/// job row was actually created (false when the slot is a skipped backlog slot, when skipped-if-
/// running, when the dedupe key collided, or when no handler exists).
async fn enqueue_due(
    db: &DatabaseConnection,
    registry: &JobRegistry,
    due: &DueSchedule,
) -> Result<bool, sea_orm::DbErr> {
    // A schedule row for a job the running binary doesn't know about (e.g. a removed Service whose
    // row lingers): the worker would just fail it "no handler registered", so skip the enqueue and
    // let the row keep advancing harmlessly.
    if registry.get(&due.job_name).is_none() {
        tracing::warn!(job_name = %due.job_name, "scheduler: no registered job for schedule; skipping");
        return Ok(false);
    }

    // The grid has already been advanced, so declining here waits for the next scheduled slot
    // rather than dropping the cadence.
    if matches!(due.catchup, CatchupPolicy::Skip) && due.missed {
        tracing::debug!(job_name = %due.job_name, "scheduler: missed slot not replayed (catchup=skip)");
        return Ok(false);
    }

    if matches!(due.overlap, OverlapPolicy::SkipIfRunning)
        && non_terminal_exists(db, &due.job_name).await?
    {
        tracing::debug!(job_name = %due.job_name, "scheduler: previous run still active; skipping (overlap=skip_if_running)");
        return Ok(false);
    }

    // Key the enqueue on (job, scheduled slot) so two replicas racing this slot collapse to one job.
    // Truncate to whole seconds so a sub-second clock difference between replicas can't split the key.
    let dedupe_key = format!("{}:{}", due.job_name, due.scheduled_at.timestamp());
    let created = worker::enqueue(
        db,
        &due.job_name,
        None,
        None,
        &serde_json::json!({
            "scheduled_at": due.scheduled_at,
            "interval_seconds": due.interval_seconds,
            "tunables": due.tunables,
        }),
        Some(&dedupe_key),
    )
    .await?;
    Ok(created.is_some())
}

/// Whether a non-terminal job of this `job_name` already exists, the skip-if-running guard. Covers
/// every pre-completion state a worker-pool or inline job can be in.
async fn non_terminal_exists(
    db: &DatabaseConnection,
    job_name: &str,
) -> Result<bool, sea_orm::DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT 1 FROM reprocessing_jobs \
             WHERE trigger_type = $1 \
               AND status IN ('queued', 'pending', 'running', 'retrying') \
             LIMIT 1",
            [job_name.into()],
        ))
        .await?;
    Ok(row.is_some())
}

/// This replica's scheduler loop: every [`TICK_SECONDS`], claim and enqueue due Services, then idle.
/// Runs until `shutdown` resolves. Mirrors the worker's biased `tokio::select!` so a shutdown is
/// observed promptly and no new work is enqueued during the drain window.
pub async fn run(
    db: DatabaseConnection,
    registry: Arc<JobRegistry>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) {
    tracing::info!(tick_secs = TICK_SECONDS, "schedule scheduler started");
    tokio::pin!(shutdown);
    let mut ticker = tokio::time::interval(Duration::from_secs(TICK_SECONDS));
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => {
                tracing::info!("scheduler stopping on shutdown");
                return;
            }
            _ = ticker.tick() => {
                match tick(&db, &registry).await {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(enqueued = n, "scheduler tick enqueued due services"),
                    Err(e) => tracing::warn!(error = %e, "scheduler tick failed"),
                }
            }
        }
    }
}
