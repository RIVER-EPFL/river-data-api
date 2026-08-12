//! Claim-based multi-replica worker pool: each replica claims a `queued` (or reapable) job with
//! `SELECT … FOR UPDATE SKIP LOCKED`, leases it, and commits ownership-guarded so a reaped stalled
//! worker can't clobber the new owner. Idempotency makes the rare overlap harmless. See ADR 0001.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use super::job::JobRegistry;
use super::lifecycle::{self, JobContext};

/// Lease lifetime before the reaper may reclaim a row. Sized well above a plausible GC / k8s
/// CPU-throttle stall so a slow-but-alive worker is not reaped mid-run.
pub const LEASE_SECONDS: i64 = 120;
/// Lease-renewal cadence, roughly one third of the lease.
pub const HEARTBEAT_SECONDS: u64 = 40;
/// Idle poll cadence when nothing is claimable.
pub const POLL_SECONDS: u64 = 2;

/// Identity for this replica's worker: pid + a short random suffix.
#[must_use]
pub fn worker_id() -> String {
    let uuid = Uuid::new_v4().to_string();
    format!("worker-{}-{}", std::process::id(), &uuid[..8])
}

/// Enqueue a `queued` job for the worker pool. A set `dedupe_key` makes the enqueue idempotent (a
/// duplicate inserts nothing and returns `None`), which keeps two replicas racing a scheduler tick
/// from double-firing one run.
pub async fn enqueue(
    db: &DatabaseConnection,
    trigger_type: &str,
    sensor_id: Option<Uuid>,
    trigger_id: Option<Uuid>,
    params: &serde_json::Value,
    dedupe_key: Option<&str>,
) -> Result<Option<Uuid>, sea_orm::DbErr> {
    let id = Uuid::new_v4();
    let category = super::registry::category_for(trigger_type);
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO reprocessing_jobs \
                 (id, trigger_type, sensor_id, trigger_id, status, category, params, dedupe_key, \
                  next_attempt_at) \
             VALUES ($1, $2, $3, $4, 'queued', $5, $6::jsonb, $7, now()) \
             ON CONFLICT (dedupe_key) WHERE dedupe_key IS NOT NULL DO NOTHING \
             RETURNING id",
            [
                id.into(),
                trigger_type.into(),
                sensor_id.into(),
                trigger_id.into(),
                category.into(),
                params.to_string().into(),
                dedupe_key.into(),
            ],
        ))
        .await?;
    row.map(|r| r.try_get("", "id")).transpose()
}

/// A row claimed off the queue.
struct Claimed {
    id: Uuid,
    trigger_type: String,
    lease_epoch: i64,
    params: serde_json::Value,
}

/// Claim one due `queued` row or one expired-lease `running` row (the reaper arm), stamping this
/// worker's ownership and a fresh lease. `SKIP LOCKED` keeps two workers from taking the same row.
async fn claim_one(
    db: &DatabaseConnection,
    worker_id: &str,
) -> Result<Option<Claimed>, sea_orm::DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "WITH claimable AS ( \
                 SELECT id FROM reprocessing_jobs \
                 WHERE (status = 'queued' AND next_attempt_at <= now()) \
                    OR (status = 'running' AND lease_expires_at < now()) \
                 ORDER BY next_attempt_at \
                 FOR UPDATE SKIP LOCKED \
                 LIMIT 1 \
             ) \
             UPDATE reprocessing_jobs j \
             SET status = 'running', \
                 owner = $1, \
                 lease_epoch = j.lease_epoch + 1, \
                 lease_expires_at = now() + (interval '1 second' * $2) \
             FROM claimable c \
             WHERE j.id = c.id \
             RETURNING j.id, j.trigger_type, j.lease_epoch, j.params",
            [worker_id.into(), LEASE_SECONDS.into()],
        ))
        .await?;

    match row {
        Some(r) => Ok(Some(Claimed {
            id: r.try_get("", "id")?,
            trigger_type: r.try_get("", "trigger_type")?,
            lease_epoch: r.try_get("", "lease_epoch")?,
            params: r.try_get("", "params")?,
        })),
        None => Ok(None),
    }
}

/// Renew the lease on a cadence while the job runs, and observe cross-replica cancellation: if
/// `cancel_requested` is set on the row, flip the in-process flag so the job's checkpoints stop; if
/// the ownership-guarded renewal matches no row (we were reaped), flip cancel and stop heartbeating.
async fn heartbeat(
    db: DatabaseConnection,
    job_id: Uuid,
    worker_id: String,
    lease_epoch: i64,
    cancel: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECONDS));
    tick.tick().await; // the immediate first tick, skip it, the claim just set the lease
    loop {
        tick.tick().await;
        let renewed = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE reprocessing_jobs \
                 SET lease_expires_at = now() + (interval '1 second' * $1) \
                 WHERE id = $2 AND owner = $3 AND lease_epoch = $4 \
                 RETURNING cancel_requested",
                [
                    LEASE_SECONDS.into(),
                    job_id.into(),
                    worker_id.clone().into(),
                    lease_epoch.into(),
                ],
            ))
            .await;
        match renewed {
            Ok(Some(r)) => {
                if r.try_get::<bool>("", "cancel_requested").unwrap_or(false) {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            // No row matched → we lost the lease (reclaimed). Stop the job and stop heartbeating.
            Ok(None) => {
                cancel.store(true, Ordering::Relaxed);
                break;
            }
            Err(e) => tracing::warn!(error = %e, job_id = %job_id, "job heartbeat failed"),
        }
    }
}

/// Mark a finished job terminal, **ownership-guarded** so a reaped-out worker's late write is a no-op.
/// Returns whether this worker still owned the row (i.e. whether the write took effect).
async fn commit_terminal(
    db: &DatabaseConnection,
    claimed: &Claimed,
    worker_id: &str,
    status: &str,
    readings_updated: Option<i32>,
    error_message: Option<&str>,
) -> Result<bool, sea_orm::DbErr> {
    let res = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE reprocessing_jobs \
             SET status = $1, readings_updated = $2, error_message = $3, completed_at = now(), \
                 owner = NULL, lease_expires_at = NULL \
             WHERE id = $4 AND owner = $5 AND lease_epoch = $6",
            [
                status.into(),
                readings_updated.into(),
                error_message.into(),
                claimed.id.into(),
                worker_id.into(),
                claimed.lease_epoch.into(),
            ],
        ))
        .await?;
    Ok(res.rows_affected() > 0)
}

/// On a retryable failure, durably reschedule (`status='pending'`, future `next_attempt_at` with
/// exponential backoff) until the retry budget is spent, then fail. Ownership-guarded. The backoff is
/// computed in SQL from the *current* `retry_count` so it survives restarts (no in-process timer).
async fn reschedule_or_fail(
    db: &DatabaseConnection,
    claimed: &Claimed,
    worker_id: &str,
    error_message: &str,
) -> Result<(), sea_orm::DbErr> {
    let policy = lifecycle::job_retry_policy();
    let max_retries = i64::from(policy.max_retries);
    let backoff_base = policy.backoff_base.as_secs() as i64;
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE reprocessing_jobs \
         SET status = CASE WHEN retry_count < $1 THEN 'queued' ELSE 'failed' END, \
             retry_count = retry_count + 1, \
             error_message = $2, \
             next_attempt_at = CASE WHEN retry_count < $1 \
                 THEN now() + (interval '1 second' * ($3 * power(2, retry_count))) \
                 ELSE next_attempt_at END, \
             completed_at = CASE WHEN retry_count < $1 THEN NULL ELSE now() END, \
             owner = NULL, lease_expires_at = NULL \
         WHERE id = $4 AND owner = $5 AND lease_epoch = $6",
        [
            max_retries.into(),
            error_message.into(),
            backoff_base.into(),
            claimed.id.into(),
            worker_id.into(),
            claimed.lease_epoch.into(),
        ],
    ))
    .await?;
    Ok(())
}

/// Run a single claimed job to its terminal (or rescheduled) state. Separated from [`run`] so tests
/// can drive one cycle deterministically.
async fn execute(
    db: &DatabaseConnection,
    events: &crate::common::EventSender,
    registry: &JobRegistry,
    worker_id: &str,
    claimed: Claimed,
) -> Result<(), sea_orm::DbErr> {
    let Some(job) = registry.get(&claimed.trigger_type) else {
        // No handler, fail rather than let the reaper reclaim it forever.
        commit_terminal(db, &claimed, worker_id, "failed", None, Some("no handler registered for trigger_type")).await?;
        return Ok(());
    };

    let (ctx, cancel) = JobContext::for_worker(db.clone(), claimed.id, events.clone(), claimed.params.clone());
    let hb = tokio::spawn(heartbeat(
        db.clone(),
        claimed.id,
        worker_id.to_string(),
        claimed.lease_epoch,
        cancel.clone(),
    ));

    // Catch a handler panic so it becomes a normal job failure instead of unwinding the worker task.
    // Without this, a panic skips `hb.abort()` below, the detached heartbeat then renews the lease
    // forever (the reaper never reclaims the row) while this replica is left with no worker.
    let run_result = std::panic::AssertUnwindSafe(job.run(ctx)).catch_unwind().await;
    hb.abort();

    let outcome = match run_result {
        Ok(result) => result,
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "job handler panicked".to_string());
            tracing::error!(job_id = %claimed.id, panic = %msg, "job handler panicked");
            Err(sea_orm::DbErr::Custom(format!("job handler panicked: {msg}")))
        }
    };

    match outcome {
        Ok(count) => {
            let status = if cancel.load(Ordering::Relaxed) {
                "cancelled"
            } else {
                "completed"
            };
            let readings = i32::try_from(count).unwrap_or(i32::MAX);
            let owned = commit_terminal(db, &claimed, worker_id, status, Some(readings), None).await?;
            if owned {
                // Mirror the inline lifecycle's post-success reconcile. Most tracked jobs rewrite
                // reading values or attribution (recalibration, reprocess, pairing/derived backfill,
                // merge, adopt/swap), any of which can change breach state. Reconcile unconditionally:
                // it is idempotent, O(active slots), spawns no jobs (no recursion), and is merely
                // redundant for jobs that don't touch values. Guarded on `owned` so only the winning
                // worker runs it.
                crate::routes::private::alarms::sweeper::reconcile_all_and_notify(db, events).await;
                let _ = events.send(crate::common::AppEvent::JobCompleted {
                    job_id: claimed.id,
                    status: status.to_string(),
                    readings_updated: Some(readings),
                    error_message: None,
                });
            }
        }
        Err(e) => {
            reschedule_or_fail(db, &claimed, worker_id, &e.to_string()).await?;
        }
    }
    Ok(())
}

/// Claim and execute at most one job. Returns `true` if a job ran, `false` if the queue was empty.
/// The unit of work tests drive directly.
pub async fn run_one(
    db: &DatabaseConnection,
    events: &crate::common::EventSender,
    registry: &JobRegistry,
    worker_id: &str,
) -> Result<bool, sea_orm::DbErr> {
    match claim_one(db, worker_id).await? {
        Some(claimed) => {
            execute(db, events, registry, worker_id, claimed).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Run claimable jobs until the queue drains. Lets a test pump the worker after enqueuing.
pub async fn drain(
    db: &DatabaseConnection,
    events: &crate::common::EventSender,
    registry: &JobRegistry,
    worker_id: &str,
) -> Result<(), sea_orm::DbErr> {
    while run_one(db, events, registry, worker_id).await? {}
    Ok(())
}

/// This replica's worker loop: drain claimable work, then idle-poll. Runs until `shutdown` resolves,
/// at which point it stops claiming and returns so in-flight work finishes within the k8s grace
/// window (anything killed is recovered by lease expiry).
pub async fn run(
    db: DatabaseConnection,
    events: crate::common::EventSender,
    registry: Arc<JobRegistry>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) {
    let wid = worker_id();
    tracing::info!(worker_id = %wid, "job worker started");
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => {
                tracing::info!(worker_id = %wid, "job worker draining on shutdown");
                return;
            }
            ran = run_one(&db, &events, &registry, &wid) => {
                match ran {
                    Ok(true) => continue, // drain: immediately try the next
                    Ok(false) => tokio::time::sleep(Duration::from_secs(POLL_SECONDS)).await,
                    Err(e) => {
                        tracing::warn!(error = %e, worker_id = %wid, "worker cycle failed");
                        tokio::time::sleep(Duration::from_secs(POLL_SECONDS)).await;
                    }
                }
            }
        }
    }
}
