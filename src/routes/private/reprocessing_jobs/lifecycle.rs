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
use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;
use uuid::Uuid;

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

fn job_retry_policy() -> RetryPolicy {
    JOB_RETRY_POLICY.get().copied().unwrap_or_default()
}

/// Reconcile tracked jobs left mid-flight by a previous process. A job still in
/// `pending`/`running`/`retrying` at startup can only be the corpse of a background task that died
/// with the last process (the spawned task does not survive a restart), so mark it `interrupted` —
/// a terminal status the UI shows as stopped and that is safe to rerun. Returns the count swept.
///
/// Must run once at startup **after** migrations and **before** anything can create a new job (the
/// janitor spawn, the HTTP server), so it can't sweep a job that legitimately just started.
pub async fn reconcile_interrupted_jobs(db: &DatabaseConnection) -> Result<u64, sea_orm::DbErr> {
    let res = db
        .execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE reprocessing_jobs \
             SET status = 'interrupted', completed_at = NOW(), \
                 error_message = 'Interrupted by API restart' \
             WHERE status IN ('pending', 'running', 'retrying')",
        ))
        .await?;
    Ok(res.rows_affected())
}

/// Handle passed to a tracked job's work closure. Owns a DB connection, the job id, and the event
/// sender so work can report incremental progress that the UI sees live. Cheap to clone (the retry
/// loop hands a fresh clone to each attempt).
#[derive(Clone)]
pub struct JobContext {
    db: DatabaseConnection,
    job_id: Uuid,
    events: crate::common::EventSender,
}

impl JobContext {
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
        "INSERT INTO reprocessing_jobs (id, sensor_id, trigger_type, trigger_id, status) \
         VALUES ($1, $2, $3, $4, 'pending')",
        [job_id.into(), sensor_id_value, trigger_type.into(), trigger_id_value],
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
        };

        let mut attempt = 0u32;
        let outcome = loop {
            match make_work(ctx.clone()).await {
                Ok(count) => break Ok(count),
                Err(e) if attempt < max_retries => {
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
                if let Err(e) = db
                    .execute(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "UPDATE reprocessing_jobs \
                         SET status = 'completed', readings_updated = $1, \
                             completed_at = NOW() \
                         WHERE id = $2",
                        [count.into(), job_id.into()],
                    ))
                    .await
                {
                    tracing::warn!(error = %e, job_id = %job_id, "Failed to mark reprocessing job completed");
                }
                // Most tracked jobs rewrite reading values or attribution (recalibration,
                // reprocess, pairing/derived backfill, adopt/swap), any of which can change breach
                // state. Reconcile unconditionally: it is idempotent, O(active slots), spawns no
                // jobs (no recursion), and is merely redundant for jobs that don't touch values.
                crate::routes::private::alarms::sweeper::reconcile_all_and_notify(&db, &events)
                    .await;
                let _ = events.send(crate::common::AppEvent::JobCompleted {
                    job_id,
                    status: "completed".into(),
                    readings_updated: Some(count as i32),
                    error_message: None,
                });
                tracing::info!(
                    readings_updated = count,
                    trigger = %trigger_type,
                    "Tracked job completed"
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
        "INSERT INTO reprocessing_jobs (id, sensor_id, trigger_type, trigger_id, status) \
         VALUES ($1, $2, $3, $4, 'running')",
        [job_id.into(), sensor_id_value, trigger_type.into(), trigger_id_value],
    ))
    .await?;
    let _ = events.send(crate::common::AppEvent::JobCreated { job_id });
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
    };

    match work(ctx).await {
        Ok(count) => {
            if let Err(e) = db
                .execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE reprocessing_jobs SET status = 'completed', readings_updated = $1, \
                     completed_at = NOW() WHERE id = $2",
                    [count.into(), job_id.into()],
                ))
                .await
            {
                tracing::warn!(error = %e, job_id = %job_id, "Failed to mark tracked job completed");
            }
            crate::routes::private::alarms::sweeper::reconcile_all_and_notify(db, &events).await;
            let _ = events.send(crate::common::AppEvent::JobCompleted {
                job_id,
                status: "completed".into(),
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
    }
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
