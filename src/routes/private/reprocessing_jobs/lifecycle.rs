//! The single tracked-job lifecycle: insert a `reprocessing_jobs` row, emit SSE events, run the
//! work closure (with retry + backoff), and record the terminal status. Every background job in the
//! API flows through here so that observability, retry, and (eventually) cancellation/rerun are
//! defined once.
//!
//! The core takes a [`JobContext`] closure so work can report incremental progress
//! ([`JobContext::set_progress`]). Simple jobs that don't need progress use the `db`-closure
//! adapters ([`spawn_tracked_job`] / [`spawn_tracked_job_with_retry`]); loop-based jobs that report
//! progress use [`spawn_tracked_job_ctx`]. Domain glue (e.g. `spawn_reprocessing_job`) wraps these.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use uuid::Uuid;

/// In-process registry of cancel flags for running jobs. A cooperative cancel sets the flag; the
/// job's loop checks [`JobContext::is_cancelled`] at its batch checkpoints and bails. Lives in the
/// process (single-replica, like the moka caches); flags are registered when a job starts and
/// removed when it reaches a terminal state.
fn cancel_registry() -> MutexGuard<'static, HashMap<Uuid, Arc<AtomicBool>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<Uuid, Arc<AtomicBool>>>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn register_cancel(job_id: Uuid) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    cancel_registry().insert(job_id, flag.clone());
    flag
}

fn deregister_cancel(job_id: Uuid) {
    cancel_registry().remove(&job_id);
}

/// Request cancellation of a running job. Returns `true` if a running job was signalled, `false` if
/// no such job is in flight in this process (already finished, or never registered).
#[must_use]
pub fn request_cancel(job_id: Uuid) -> bool {
    match cancel_registry().get(&job_id) {
        Some(flag) => {
            flag.store(true, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// Retry policy for tracked jobs. Set once at startup from `Config`; code paths that don't run
/// `main.rs` (integration tests) see the default — no retries — so their behavior is unchanged.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub backoff_base: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 0,
            backoff_base: Duration::from_secs(60),
        }
    }
}

static JOB_RETRY_POLICY: OnceLock<RetryPolicy> = OnceLock::new();

/// Initialise the global tracked-job retry policy (call once at startup, from `main.rs`).
pub fn set_job_retry_policy(policy: RetryPolicy) {
    let _ = JOB_RETRY_POLICY.set(policy);
}

pub(crate) fn job_retry_policy() -> RetryPolicy {
    JOB_RETRY_POLICY.get().copied().unwrap_or_default()
}

/// Stable identity for THIS replica, stamped as `owner` on inline-spawned jobs. The startup sweep is
/// scoped to it so a booting replica only reconciles rows it (the same pod) created — never a peer's
/// live inline job. In k8s this is the pod name (`HOSTNAME`): unique per replica, stable across a
/// container restart so a crashed-and-restarted pod still recovers its own orphans; a different pod
/// (rolling deploy) gets a different id and leaves prior orphans for the leased worker pool. Falls
/// back to the pid when HOSTNAME is unset (local runs).
#[must_use]
pub fn process_owner() -> &'static str {
    static OWNER: OnceLock<String> = OnceLock::new();
    OWNER.get_or_init(|| {
        std::env::var("HOSTNAME")
            .ok()
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| format!("pid-{}", std::process::id()))
    })
}

/// Sweep inline jobs stranded mid-flight by a dead process to `interrupted` at startup. Only
/// lease-less rows owned by THIS replica qualify — worker-pool jobs carry a lease (recovered by the
/// reaper) and a peer's live inline jobs carry a different `owner`, so a restart can't strand or
/// double-claim them. Runs after migrations, before serving.
pub async fn reconcile_interrupted_jobs(db: &DatabaseConnection) -> Result<u64, sea_orm::DbErr> {
    let res = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE reprocessing_jobs \
             SET status = 'interrupted', completed_at = NOW(), \
                 error_message = 'Interrupted by API restart' \
             WHERE status IN ('pending', 'running', 'retrying') AND lease_expires_at IS NULL \
               AND owner = $1",
            [process_owner().into()],
        ))
        .await?;
    Ok(res.rows_affected())
}

/// Handle passed to a tracked job's work closure. Owns a DB connection, the job id, and the event
/// sender so work can report incremental progress, structured `detail`, and a timeline of log lines
/// that the UI sees live. Cheap to clone (the retry loop hands a fresh clone to each attempt); the
/// log `seq` counter is shared across clones so ordering is monotonic across retries.
#[derive(Clone)]
pub struct JobContext {
    db: DatabaseConnection,
    job_id: Uuid,
    events: crate::common::EventSender,
    seq: Arc<AtomicI64>,
    cancel: Arc<AtomicBool>,
    params: serde_json::Value,
}

impl JobContext {
    /// Build a context for a job claimed by the worker pool. Returns the context plus the in-process
    /// cancel flag the worker's heartbeat flips when it sees `cancel_requested` on the row (or when
    /// the lease is lost), so a cooperatively-cancellable job stops at its next checkpoint.
    pub(crate) fn for_worker(
        db: DatabaseConnection,
        job_id: Uuid,
        events: crate::common::EventSender,
        params: serde_json::Value,
    ) -> (Self, Arc<AtomicBool>) {
        let cancel = Arc::new(AtomicBool::new(false));
        let ctx = Self {
            db,
            job_id,
            events,
            seq: Arc::new(AtomicI64::new(0)),
            cancel: cancel.clone(),
            params,
        };
        (ctx, cancel)
    }

    /// The job's persisted inputs — what a worker-run job reads to do its work.
    #[must_use]
    pub fn params(&self) -> &serde_json::Value {
        &self.params
    }

    /// The DB connection the job should use.
    #[must_use]
    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }

    /// This job's id.
    #[must_use]
    pub fn job_id(&self) -> Uuid {
        self.job_id
    }

    /// Whether cancellation has been requested. Loop-based work checks this at its batch
    /// checkpoints and returns early; the lifecycle then records the job as `cancelled`.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Append a line to the job's timeline (`reprocessing_job_logs`). `warn`/`error` lines are also
    /// streamed over SSE as `JobLog`; the full ordered timeline is fetched on demand from
    /// `GET /api/jobs/{id}/logs`. Best-effort — a logging failure must never fail the job.
    pub async fn log(&self, level: &str, message: &str, context: serde_json::Value) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = self
            .db
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "INSERT INTO reprocessing_job_logs (job_id, seq, level, message, context) \
                 VALUES ($1, $2, $3, $4, $5::jsonb)",
                [
                    self.job_id.into(),
                    seq.into(),
                    level.into(),
                    message.into(),
                    context.to_string().into(),
                ],
            ))
            .await
        {
            tracing::warn!(error = %e, job_id = %self.job_id, "Failed to append job log line");
        }
        if level == "warn" || level == "error" {
            let _ = self.events.send(crate::common::AppEvent::JobLog {
                job_id: self.job_id,
                seq,
                level: level.into(),
                message: message.into(),
                context,
            });
        }
    }

    /// Convenience: an `info` timeline line with no structured context.
    pub async fn info(&self, message: &str) {
        self.log("info", message, serde_json::json!({})).await;
    }

    /// Replace the job's structured `detail` summary (scope, time range, counts, provenance).
    /// Best-effort.
    pub async fn set_detail(&self, detail: serde_json::Value) {
        if let Err(e) = self
            .db
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE reprocessing_jobs SET detail = $1::jsonb WHERE id = $2",
                [detail.to_string().into(), self.job_id.into()],
            ))
            .await
        {
            tracing::warn!(error = %e, job_id = %self.job_id, "Failed to set job detail");
        }
    }

    /// Set the job's `site_id` scope column (promoted from `detail` for list filtering).
    pub async fn set_site(&self, site_id: Uuid) {
        if let Err(e) = self
            .db
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE reprocessing_jobs SET site_id = $1 WHERE id = $2",
                [site_id.into(), self.job_id.into()],
            ))
            .await
        {
            tracing::warn!(error = %e, job_id = %self.job_id, "Failed to set job site_id");
        }
    }

    /// Atomically persist `progress` (and `total` when provided) onto the row **and** emit the
    /// matching `JobProgress` event, so the stored row and the live SSE stream never disagree and a
    /// crash leaves a truthful last-known checkpoint. Best-effort: a failed write is logged, never
    /// fatal to the job.
    pub async fn set_progress(&self, progress: i32, total: Option<i32>) {
        let stmt = match total {
            Some(t) => Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE reprocessing_jobs SET progress = $1, total = $2 WHERE id = $3",
                [progress.into(), t.into(), self.job_id.into()],
            ),
            None => Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE reprocessing_jobs SET progress = $1 WHERE id = $2",
                [progress.into(), self.job_id.into()],
            ),
        };
        if let Err(e) = self.db.execute(stmt).await {
            tracing::warn!(error = %e, job_id = %self.job_id, "Failed to update job progress");
        }
        let _ = self.events.send(crate::common::AppEvent::JobProgress {
            job_id: self.job_id,
            status: "running".into(),
            progress: Some(progress),
            total,
        });
    }
}

/// Core tracked-job lifecycle (JobContext closure + explicit retry policy). Inserts a
/// `reprocessing_jobs` row (`status = 'pending'`), emits `JobCreated`, then runs `make_work` in a
/// background task that flips the row through `running` → `completed`/`failed`. On a failed attempt
/// the row is set to `retrying` (bumping `retry_count`) and `make_work` is re-invoked after an
/// exponential backoff; only after `max_retries` exhausted is it marked `failed`. `make_work` must
/// be `Fn` (callable once per attempt). Returns the job id immediately.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_tracked_job_ctx_with_retry<F, Fut>(
    db: &DatabaseConnection,
    sensor_id: Option<Uuid>,
    trigger_type: &str,
    trigger_id: Option<Uuid>,
    events: crate::common::EventSender,
    max_retries: u32,
    backoff_base: Duration,
    make_work: F,
) -> Result<Uuid, sea_orm::DbErr>
where
    F: Fn(JobContext) -> Fut + Send + 'static,
    Fut: Future<Output = Result<i64, sea_orm::DbErr>> + Send,
{
    use sea_orm::Value;

    let job_id = Uuid::new_v4();
    let sensor_id_value: Value = match sensor_id {
        Some(id) => id.into(),
        None => Value::Uuid(None),
    };
    let trigger_id_value: Value = match trigger_id {
        Some(id) => id.into(),
        None => Value::Uuid(None),
    };

    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO reprocessing_jobs (id, sensor_id, trigger_type, trigger_id, status, category, owner) \
         VALUES ($1, $2, $3, $4, 'pending', $5, $6)",
        [
            job_id.into(),
            sensor_id_value,
            trigger_type.into(),
            trigger_id_value,
            super::registry::category_for(trigger_type).into(),
            process_owner().into(),
        ],
    ))
    .await?;

    let _ = events.send(crate::common::AppEvent::JobCreated { job_id });

    let db = db.clone();
    let trigger_type = trigger_type.to_string();
    let events = events.clone();
    tokio::spawn(async move {
        if let Err(e) = db
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE reprocessing_jobs SET status = 'running' WHERE id = $1",
                [job_id.into()],
            ))
            .await
        {
            tracing::warn!(error = %e, job_id = %job_id, "Failed to set reprocessing job to running");
        }

        let _ = events.send(crate::common::AppEvent::JobProgress {
            job_id,
            status: "running".into(),
            progress: Some(0),
            total: None,
        });

        let ctx = JobContext {
            db: db.clone(),
            job_id,
            events: events.clone(),
            seq: Arc::new(AtomicI64::new(0)),
            cancel: register_cancel(job_id),
            params: serde_json::json!({}),
        };

        let mut attempt = 0u32;
        let outcome = loop {
            match make_work(ctx.clone()).await {
                Ok(count) => break Ok(count),
                // Don't retry a job that was cancelled mid-attempt.
                Err(e) if attempt < max_retries && !ctx.is_cancelled() => {
                    attempt += 1;
                    let msg = e.to_string();
                    let delay = backoff_base.saturating_mul(2u32.saturating_pow(attempt - 1));
                    tracing::warn!(
                        error = %e,
                        attempt,
                        max_retries,
                        trigger = %trigger_type,
                        delay_ms = delay.as_millis() as u64,
                        "Tracked job attempt failed; retrying"
                    );
                    let _ = db
                        .execute(Statement::from_sql_and_values(
                            sea_orm::DatabaseBackend::Postgres,
                            "UPDATE reprocessing_jobs SET status = 'retrying', error_message = $1, retry_count = $2 WHERE id = $3",
                            [msg.as_str().into(), (attempt as i32).into(), job_id.into()],
                        ))
                        .await;
                    let _ = events.send(crate::common::AppEvent::JobProgress {
                        job_id,
                        status: "retrying".into(),
                        progress: Some(attempt as i32),
                        total: Some(max_retries as i32),
                    });
                    tokio::time::sleep(delay).await;
                    let _ = db
                        .execute(Statement::from_sql_and_values(
                            sea_orm::DatabaseBackend::Postgres,
                            "UPDATE reprocessing_jobs SET status = 'running' WHERE id = $1",
                            [job_id.into()],
                        ))
                        .await;
                }
                Err(e) => break Err(e),
            }
        };

        match outcome {
            Ok(count) => {
                // A job that observed the cancel flag and returned early is `cancelled`, not
                // `completed`. Its partial result is consistent (every op is idempotent) and the
                // janitor finishes anything left.
                let final_status = if ctx.is_cancelled() { "cancelled" } else { "completed" };
                if let Err(e) = db
                    .execute(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "UPDATE reprocessing_jobs \
                         SET status = $1, readings_updated = $2, completed_at = NOW() \
                         WHERE id = $3",
                        [final_status.into(), count.into(), job_id.into()],
                    ))
                    .await
                {
                    tracing::warn!(error = %e, job_id = %job_id, "Failed to mark reprocessing job terminal");
                }
                // Most tracked jobs rewrite reading values or attribution (recalibration,
                // reprocess, pairing/derived backfill, adopt/swap), any of which can change breach
                // state. Reconcile unconditionally: it is idempotent, O(active slots), spawns no
                // jobs (no recursion), and is merely redundant for jobs that don't touch values.
                crate::routes::private::alarms::sweeper::reconcile_all_and_notify(&db, &events)
                    .await;
                let _ = events.send(crate::common::AppEvent::JobCompleted {
                    job_id,
                    status: final_status.into(),
                    readings_updated: Some(count as i32),
                    error_message: None,
                });
                tracing::info!(
                    readings_updated = count,
                    trigger = %trigger_type,
                    status = final_status,
                    "Tracked job finished"
                );
            }
            Err(e) => {
                let msg = e.to_string();
                if let Err(db_err) = db
                    .execute(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "UPDATE reprocessing_jobs \
                         SET status = 'failed', error_message = $1, \
                             completed_at = NOW() \
                         WHERE id = $2",
                        [msg.as_str().into(), job_id.into()],
                    ))
                    .await
                {
                    tracing::warn!(error = %db_err, job_id = %job_id, "Failed to mark reprocessing job failed");
                }
                let _ = events.send(crate::common::AppEvent::JobCompleted {
                    job_id,
                    status: "failed".into(),
                    readings_updated: None,
                    error_message: Some(msg.clone()),
                });
                tracing::error!(
                    error = %e,
                    trigger = %trigger_type,
                    "Tracked job failed"
                );
            }
        }
        deregister_cancel(job_id);
    });

    Ok(job_id)
}

/// [`spawn_tracked_job_ctx_with_retry`] using the process-wide retry policy. Use this for
/// loop-based jobs that report progress via [`JobContext::set_progress`].
pub async fn spawn_tracked_job_ctx<F, Fut>(
    db: &DatabaseConnection,
    sensor_id: Option<Uuid>,
    trigger_type: &str,
    trigger_id: Option<Uuid>,
    events: crate::common::EventSender,
    make_work: F,
) -> Result<Uuid, sea_orm::DbErr>
where
    F: Fn(JobContext) -> Fut + Send + 'static,
    Fut: Future<Output = Result<i64, sea_orm::DbErr>> + Send,
{
    let policy = job_retry_policy();
    spawn_tracked_job_ctx_with_retry(
        db,
        sensor_id,
        trigger_type,
        trigger_id,
        events,
        policy.max_retries,
        policy.backoff_base,
        make_work,
    )
    .await
}

/// Synchronous sibling of [`spawn_tracked_job_ctx`] for jobs that must run **inline** and hand their
/// result back to the caller (e.g. the periodic derived janitor, whose periodic loop awaits the
/// filled count before deciding follow-up work). Runs the full lifecycle — inserts the row as
/// `running`, emits `JobCreated`/`JobProgress`, runs `work` to completion, records
/// `completed`/`failed`, and on success runs the same post-success alarm reconcile as the spawned
/// path — but in the current task, with no retry. Returns the work's count (or its error).
pub async fn run_tracked_job<F, Fut>(
    db: &DatabaseConnection,
    sensor_id: Option<Uuid>,
    trigger_type: &str,
    trigger_id: Option<Uuid>,
    events: crate::common::EventSender,
    work: F,
) -> Result<i64, sea_orm::DbErr>
where
    F: FnOnce(JobContext) -> Fut,
    Fut: Future<Output = Result<i64, sea_orm::DbErr>>,
{
    use sea_orm::Value;

    let job_id = Uuid::new_v4();
    let sensor_id_value: Value = match sensor_id {
        Some(id) => id.into(),
        None => Value::Uuid(None),
    };
    let trigger_id_value: Value = match trigger_id {
        Some(id) => id.into(),
        None => Value::Uuid(None),
    };

    // Runs inline and starts immediately, so insert straight as `running` (no queued `pending`).
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO reprocessing_jobs (id, sensor_id, trigger_type, trigger_id, status, category, owner) \
         VALUES ($1, $2, $3, $4, 'running', $5, $6)",
        [
            job_id.into(),
            sensor_id_value,
            trigger_type.into(),
            trigger_id_value,
            super::registry::category_for(trigger_type).into(),
            process_owner().into(),
        ],
    ))
    .await?;
    let _ = events.send(crate::common::AppEvent::JobCreated { job_id });
    let _ = events.send(crate::common::AppEvent::JobProgress {
        job_id,
        status: "running".into(),
        progress: Some(0),
        total: None,
    });

    let cancel = register_cancel(job_id);
    let ctx = JobContext {
        db: db.clone(),
        job_id,
        events: events.clone(),
        seq: Arc::new(AtomicI64::new(0)),
        cancel: cancel.clone(),
        params: serde_json::json!({}),
    };

    let result = match work(ctx).await {
        Ok(count) => {
            let final_status = if cancel.load(Ordering::Relaxed) {
                "cancelled"
            } else {
                "completed"
            };
            if let Err(e) = db
                .execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE reprocessing_jobs SET status = $1, readings_updated = $2, \
                     completed_at = NOW() WHERE id = $3",
                    [final_status.into(), count.into(), job_id.into()],
                ))
                .await
            {
                tracing::warn!(error = %e, job_id = %job_id, "Failed to mark tracked job terminal");
            }
            crate::routes::private::alarms::sweeper::reconcile_all_and_notify(db, &events).await;
            let _ = events.send(crate::common::AppEvent::JobCompleted {
                job_id,
                status: final_status.into(),
                readings_updated: Some(count as i32),
                error_message: None,
            });
            Ok(count)
        }
        Err(e) => {
            let msg = e.to_string();
            if let Err(db_err) = db
                .execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE reprocessing_jobs SET status = 'failed', error_message = $1, \
                     completed_at = NOW() WHERE id = $2",
                    [msg.as_str().into(), job_id.into()],
                ))
                .await
            {
                tracing::warn!(error = %db_err, job_id = %job_id, "Failed to mark tracked job failed");
            }
            let _ = events.send(crate::common::AppEvent::JobCompleted {
                job_id,
                status: "failed".into(),
                readings_updated: None,
                error_message: Some(msg),
            });
            Err(e)
        }
    };
    deregister_cancel(job_id);
    result
}

/// Convenience adapter for jobs that only need a `DatabaseConnection` (no progress reporting), using
/// the process-wide retry policy.
pub async fn spawn_tracked_job<F, Fut>(
    db: &DatabaseConnection,
    sensor_id: Option<Uuid>,
    trigger_type: &str,
    trigger_id: Option<Uuid>,
    events: crate::common::EventSender,
    work: F,
) -> Result<Uuid, sea_orm::DbErr>
where
    F: Fn(DatabaseConnection) -> Fut + Send + 'static,
    Fut: Future<Output = Result<i64, sea_orm::DbErr>> + Send,
{
    spawn_tracked_job_ctx(db, sensor_id, trigger_type, trigger_id, events, move |ctx| {
        work(ctx.db().clone())
    })
    .await
}

/// `db`-closure adapter for [`spawn_tracked_job_ctx_with_retry`] with an explicit retry policy.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_tracked_job_with_retry<F, Fut>(
    db: &DatabaseConnection,
    sensor_id: Option<Uuid>,
    trigger_type: &str,
    trigger_id: Option<Uuid>,
    events: crate::common::EventSender,
    max_retries: u32,
    backoff_base: Duration,
    make_work: F,
) -> Result<Uuid, sea_orm::DbErr>
where
    F: Fn(DatabaseConnection) -> Fut + Send + 'static,
    Fut: Future<Output = Result<i64, sea_orm::DbErr>> + Send,
{
    spawn_tracked_job_ctx_with_retry(
        db,
        sensor_id,
        trigger_type,
        trigger_id,
        events,
        max_retries,
        backoff_base,
        move |ctx| make_work(ctx.db().clone()),
    )
    .await
}
