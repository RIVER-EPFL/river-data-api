//! Claim-based multi-replica worker pool: each replica claims a `queued` (or reapable) job with
//! `SELECT … FOR UPDATE SKIP LOCKED`, leases it, and commits ownership-guarded so a reaped stalled
//! worker can't clobber the new owner. Idempotency makes the rare overlap harmless. See ADR 0001.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::FutureExt;
use sea_orm::sea_query::{Expr, ExprTrait as _, LockBehavior, LockType, OnConflict};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, FromQueryResult, QueryFilter,
    QueryOrder, QuerySelect, Set, TransactionTrait, TryInsertResult,
};
use uuid::Uuid;

use super::job::JobRegistry;
use super::model::{ActiveModel, Column, Entity};
use super::lifecycle::{self, JobContext, RetryPolicy};

/// Lease lifetime before the reaper may reclaim a row. Sized well above a plausible GC / k8s
/// CPU-throttle stall so a slow-but-alive worker is not reaped mid-run.
pub const LEASE_SECONDS: i64 = 120;

/// When a lease taken now lapses. Server-side `now()`, so a worker whose clock has drifted cannot
/// hold a row past the reaper's reach or hand it over early.
/// The ownership guard every write a running worker makes carries: the row, this worker, and the
/// lease it was granted. A worker reaped out matches no row and its late write is a no-op.
fn owned_by(job_id: Uuid, worker_id: &str, lease_epoch: i64) -> Expr {
    Column::Id
        .eq(job_id)
        .and(Column::Owner.eq(worker_id))
        .and(Column::LeaseEpoch.eq(lease_epoch))
}

fn lease_expiry() -> Expr {
    Expr::cust_with_values(
        "now() + (interval '1 second' * $1)",
        [sea_orm::Value::from(LEASE_SECONDS)],
    )
}
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
///
/// Called from a CRUD hook, `db` is the transaction the write runs in: the row is invisible to the
/// pool until that commits, and a write that fails takes its job with it.
pub async fn enqueue<C: ConnectionTrait>(
    db: &C,
    trigger_type: &str,
    sensor_id: Option<Uuid>,
    trigger_id: Option<Uuid>,
    params: &serde_json::Value,
    dedupe_key: Option<&str>,
) -> Result<Option<Uuid>, sea_orm::DbErr> {
    let id = Uuid::new_v4();
    let category = super::registry::category_for(trigger_type);
    // The conflict target carries the index's own predicate: the unique index on `dedupe_key` is
    // partial, so naming the column alone matches no index.
    let inserted = Entity::insert(ActiveModel {
        id: Set(id),
        trigger_type: Set(trigger_type.to_string()),
        sensor_id: Set(sensor_id),
        trigger_id: Set(trigger_id),
        status: Set("queued".to_string()),
        category: Set(category.to_string()),
        params: Set(params.clone()),
        dedupe_key: Set(dedupe_key.map(ToString::to_string)),
        next_attempt_at: Set(chrono::Utc::now().into()),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(Column::DedupeKey)
            .target_and_where(Expr::col(Column::DedupeKey).is_not_null())
            .do_nothing()
            .to_owned(),
    )
    .try_insert()
    .exec(db)
    .await?;
    Ok(match inserted {
        TryInsertResult::Inserted(_) => Some(id),
        TryInsertResult::Conflicted | TryInsertResult::Empty => None,
    })
}

/// A row claimed off the queue.
#[derive(FromQueryResult)]
struct Claimed {
    id: Uuid,
    trigger_type: String,
    lease_epoch: i64,
    params: serde_json::Value,
    /// Attempts already spent on this row, so the timeline says which try it is watching.
    retry_count: i32,
}

/// Claim one due `queued` row or one orphaned `running` row (the reaper arm), stamping this
/// worker's ownership and a fresh lease. A claimed row always carries a lease, so a `running` row
/// with none is an orphan too: rows stranded by the pre-worker-pool spawn path have no lease at all. `SKIP LOCKED` keeps two workers from taking the same row.
/// The claim releases `dedupe_key`: the key coalesces enqueues while a job waits, and a change
/// landing once the run has started needs a run of its own.
async fn claim_one(
    db: &DatabaseConnection,
    worker_id: &str,
) -> Result<Option<Claimed>, sea_orm::DbErr> {
    // The select and the update are one transaction because the row lock is what keeps two
    // workers off the same row: `SKIP LOCKED` holds it until this transaction commits, so a
    // second worker's select passes over it rather than waiting for it.
    let txn = db.begin().await?;
    let now = || Expr::current_timestamp();
    let due = Column::Status
        .eq("queued")
        .and(Expr::col(Column::NextAttemptAt).lte(now()));
    let orphaned = Column::Status.eq("running").and(
        Column::LeaseExpiresAt
            .is_null()
            .or(Expr::col(Column::LeaseExpiresAt).lt(now())),
    );
    let Some(row) = Entity::find()
        .filter(due.or(orphaned))
        .order_by_asc(Column::NextAttemptAt)
        .limit(1)
        .lock_with_behavior(LockType::Update, LockBehavior::SkipLocked)
        .one(&txn)
        .await?
    else {
        txn.commit().await?;
        return Ok(None);
    };
    let lease_epoch = row.lease_epoch + 1;
    Entity::update_many()
        .col_expr(Column::Status, Expr::value("running"))
        .col_expr(Column::Owner, Expr::value(worker_id))
        .col_expr(Column::DedupeKey, Expr::value(Option::<String>::None))
        .col_expr(Column::LeaseEpoch, Expr::value(lease_epoch))
        .col_expr(Column::LeaseExpiresAt, lease_expiry())
        .filter(Column::Id.eq(row.id))
        .exec(&txn)
        .await?;
    txn.commit().await?;
    Ok(Some(Claimed {
        id: row.id,
        trigger_type: row.trigger_type,
        lease_epoch,
        params: row.params,
        retry_count: row.retry_count,
    }))
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
        // Renew and read the cancel flag as two statements: the renewal is what proves ownership,
        // and cancellation is advisory, re-read on the next tick if this read misses it.
        let renewed = Entity::update_many()
            .col_expr(Column::LeaseExpiresAt, lease_expiry())
            .filter(owned_by(job_id, &worker_id, lease_epoch))
            .exec(&db)
            .await;
        match renewed {
            Ok(res) if res.rows_affected > 0 => {
                match Entity::find_by_id(job_id).one(&db).await {
                    Ok(Some(row)) if row.cancel_requested => cancel.store(true, Ordering::Relaxed),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(job_id = %job_id, error = %e, "cancel flag unreadable"),
                }
            }
            // No row matched → we lost the lease (reclaimed). Stop the job and stop heartbeating.
            Ok(_) => {
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
    let res = Entity::update_many()
        .col_expr(Column::Status, Expr::value(status))
        .col_expr(Column::ReadingsUpdated, Expr::value(readings_updated))
        .col_expr(Column::ErrorMessage, Expr::value(error_message))
        .col_expr(Column::CompletedAt, Expr::current_timestamp())
        .col_expr(Column::Owner, Expr::value(Option::<String>::None))
        .col_expr(
            Column::LeaseExpiresAt,
            Expr::value(Option::<sea_orm::prelude::DateTimeWithTimeZone>::None),
        )
        .filter(owned_by(claimed.id, worker_id, claimed.lease_epoch))
        .exec(db)
        .await?;
    Ok(res.rows_affected > 0)
}

/// On a retryable failure, durably reschedule (`status='queued'`, future `next_attempt_at` with
/// exponential backoff) until the retry budget is spent, then fail. Ownership-guarded. The backoff is
/// computed in SQL from the *current* `retry_count` so it survives restarts (no in-process timer).
/// Returns the status the row landed on, or `None` when another worker owned it.
async fn reschedule_or_fail(
    db: &DatabaseConnection,
    claimed: &Claimed,
    worker_id: &str,
    policy: RetryPolicy,
    error_message: &str,
) -> Result<Option<String>, sea_orm::DbErr> {
    let max_retries = i64::from(policy.max_retries);
    let backoff_base = policy.backoff_base.as_secs() as i64;
    // The backoff is read off the row's own `retry_count` rather than the claim's, so a restart
    // between the claim and the failure still doubles from where the row stands.
    let retrying = Expr::col(Column::RetryCount).lt(max_retries);
    let res = Entity::update_many()
        .col_expr(
            Column::Status,
            Expr::case(retrying.clone(), "queued").finally("failed").into(),
        )
        .col_expr(
            Column::RetryCount,
            Expr::col(Column::RetryCount).add(Expr::value(1)),
        )
        .col_expr(Column::ErrorMessage, Expr::value(error_message))
        .col_expr(
            Column::NextAttemptAt,
            Expr::case(
                retrying.clone(),
                Expr::cust_with_values(
                    "now() + (interval '1 second' * ($1 * power(2, retry_count)))",
                    [sea_orm::Value::from(backoff_base)],
                ),
            )
            .finally(Expr::col(Column::NextAttemptAt))
            .into(),
        )
        .col_expr(
            Column::CompletedAt,
            Expr::case(
                retrying,
                Expr::value(Option::<sea_orm::prelude::DateTimeWithTimeZone>::None),
            )
            .finally(Expr::current_timestamp())
            .into(),
        )
        .col_expr(Column::Owner, Expr::value(Option::<String>::None))
        .col_expr(
            Column::LeaseExpiresAt,
            Expr::value(Option::<sea_orm::prelude::DateTimeWithTimeZone>::None),
        )
        .filter(owned_by(claimed.id, worker_id, claimed.lease_epoch))
        .exec(db)
        .await?;
    if res.rows_affected == 0 {
        return Ok(None);
    }
    Ok(Some(
        if i64::from(claimed.retry_count) < max_retries {
            "queued"
        } else {
            "failed"
        }
        .to_string(),
    ))
}

/// Run a single claimed job to its terminal (or rescheduled) state. Separated from [`run`] so tests
/// can drive one cycle deterministically.
async fn execute(
    db: &DatabaseConnection,
    events: &crate::common::EventSender,
    registry: &JobRegistry,
    worker_id: &str,
    policy: RetryPolicy,
    claimed: Claimed,
) -> Result<(), sea_orm::DbErr> {
    let Some(job) = registry.get(&claimed.trigger_type) else {
        // No handler, fail rather than let the reaper reclaim it forever.
        const NO_HANDLER: &str = "no handler registered for trigger_type";
        if commit_terminal(db, &claimed, worker_id, "failed", None, Some(NO_HANDLER)).await? {
            let _ = events.send(crate::common::AppEvent::JobCompleted {
                job_id: claimed.id,
                status: "failed".to_string(),
                readings_updated: None,
                error_message: Some(NO_HANDLER.to_string()),
            });
        }
        return Ok(());
    };

    let (ctx, cancel) = JobContext::for_worker(
        db.clone(),
        claimed.id,
        events.clone(),
        claimed.params.clone(),
    );
    // The two lines every run owes its timeline. A job body says what only it knows; that a run
    // started and how it ended is the worker's to say, so a silent job is impossible.
    let timeline = ctx.clone();
    timeline
        .log(
            "info",
            &format!("{} started", claimed.trigger_type),
            serde_json::json!({
                "trigger_type": claimed.trigger_type,
                "attempt": claimed.retry_count + 1,
            }),
        )
        .await;
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
    let run_result = std::panic::AssertUnwindSafe(job.run(ctx))
        .catch_unwind()
        .await;
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
            Err(sea_orm::DbErr::Custom(format!(
                "job handler panicked: {msg}"
            )))
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
            timeline
                .log(
                    "info",
                    &format!("{} {status}", claimed.trigger_type),
                    serde_json::json!({ "status": status, "reported": count }),
                )
                .await;
            let owned =
                commit_terminal(db, &claimed, worker_id, status, Some(readings), None).await?;
            if owned {
                // Most tracked jobs rewrite reading values or attribution (recalibration, reprocess,
                // pairing/derived backfill, merge, adopt/swap), any of which can change breach
                // state. Reconcile unconditionally:
                // it is idempotent, O(active slots), spawns no jobs (no recursion), and is merely
                // redundant for jobs that don't touch values. Guarded on `owned` so only the winning
                // worker runs it.
                crate::routes::private::alarms::flows::reconcile_all_and_notify(db, events).await;
                let _ = events.send(crate::common::AppEvent::JobCompleted {
                    job_id: claimed.id,
                    status: status.to_string(),
                    readings_updated: Some(readings),
                    error_message: None,
                });
            }
        }
        Err(e) => {
            let message = e.to_string();
            let outcome = if policy.max_retries > u32::try_from(claimed.retry_count).unwrap_or(0) {
                "retrying"
            } else {
                "failed"
            };
            timeline
                .log(
                    if outcome == "failed" { "error" } else { "warn" },
                    &format!("{} {outcome}", claimed.trigger_type),
                    serde_json::json!({ "status": outcome, "error": message }),
                )
                .await;
            // Announce the failure the way the success arm announces completion: a watcher that only
            // sees `JobCompleted` learns nothing of a run that failed or is waiting out its backoff.
            match reschedule_or_fail(db, &claimed, worker_id, policy, &message)
                .await?
                .as_deref()
            {
                Some("failed") => {
                    let _ = events.send(crate::common::AppEvent::JobCompleted {
                        job_id: claimed.id,
                        status: "failed".to_string(),
                        readings_updated: None,
                        error_message: Some(message),
                    });
                }
                Some(_) => {
                    let _ = events.send(crate::common::AppEvent::JobProgress {
                        job_id: claimed.id,
                        status: "retrying".to_string(),
                        progress: None,
                        total: None,
                    });
                }
                None => {}
            }
        }
    }
    Ok(())
}

/// Claim and execute at most one job under the process-wide retry policy. Returns `true` if a job
/// ran, `false` if the queue was empty. The unit of work tests drive directly.
pub async fn run_one(
    db: &DatabaseConnection,
    events: &crate::common::EventSender,
    registry: &JobRegistry,
    worker_id: &str,
) -> Result<bool, sea_orm::DbErr> {
    run_one_with_policy(
        db,
        events,
        registry,
        worker_id,
        lifecycle::job_retry_policy(),
    )
    .await
}

/// [`run_one`] with an explicit retry policy, for a caller that must not depend on the process-wide
/// one.
pub async fn run_one_with_policy(
    db: &DatabaseConnection,
    events: &crate::common::EventSender,
    registry: &JobRegistry,
    worker_id: &str,
    policy: RetryPolicy,
) -> Result<bool, sea_orm::DbErr> {
    match claim_one(db, worker_id).await? {
        Some(claimed) => {
            execute(db, events, registry, worker_id, policy, claimed).await?;
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

/// This replica's worker loop: drain claimable work, then idle-poll. `shutdown` stops the worker
/// claiming anything new; the job already claimed runs to completion first, so a rolled pod hands
/// back finished work rather than a part-written row holding its lease until the reaper takes it.
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
        // Never race the shutdown against the work: `select!` drops the loser, which would abandon
        // a claimed job at whatever await point it had reached.
        let idle = match run_one(&db, &events, &registry, &wid).await {
            Ok(ran) => !ran,
            Err(e) => {
                tracing::warn!(error = %e, worker_id = %wid, "worker cycle failed");
                true
            }
        };
        if idle {
            tokio::select! {
                biased;
                () = &mut shutdown => {
                    tracing::info!(worker_id = %wid, "job worker stopping on shutdown");
                    return;
                }
                () = tokio::time::sleep(Duration::from_secs(POLL_SECONDS)) => {}
            }
        } else if (&mut shutdown).now_or_never().is_some() {
            tracing::info!(worker_id = %wid, "job worker stopping on shutdown, claimed job finished");
            return;
        }
    }
}
