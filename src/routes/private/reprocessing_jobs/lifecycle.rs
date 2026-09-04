//! What a claimed job runs with: the [`JobContext`] handed to `Job::run` (progress, structured
//! `detail`, the timeline in `reprocessing_job_logs`, the cancel flag) and the process-wide
//! [`RetryPolicy`] the worker pool reschedules a failed run under. Rows are created by
//! `worker::enqueue` and driven by the worker pool; nothing here spawns work.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;
use uuid::Uuid;

/// Retry policy the worker pool reschedules a failed run under. Set once at startup from `Config`;
/// code paths that don't run `main.rs` (integration tests) see the default, no retries.
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

/// Handle passed to `Job::run`. Owns a DB connection, the job id, and the event sender so work can
/// report incremental progress, structured `detail`, and a timeline of log lines that the UI sees
/// live. Cheap to clone; the log `seq` counter is shared across clones so ordering stays monotonic.
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

    /// The job's persisted inputs, what a worker-run job reads to do its work.
    #[must_use]
    pub fn params(&self) -> &serde_json::Value {
        &self.params
    }

    /// The DB connection the job should use.
    #[must_use]
    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }

    /// The SSE event sender, for jobs that emit domain events (e.g. `DataIngested`) beyond the
    /// lifecycle's own progress/completion events.
    #[must_use]
    pub fn events(&self) -> &crate::common::EventSender {
        &self.events
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
    /// `GET /api/jobs/{id}/logs`. Best-effort, a logging failure must never fail the job.
    pub async fn log(&self, level: &str, message: &str, context: serde_json::Value) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = self
            .db
            .execute_raw(Statement::from_sql_and_values(
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
            .execute_raw(Statement::from_sql_and_values(
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
            .execute_raw(Statement::from_sql_and_values(
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
        if let Err(e) = self.db.execute_raw(stmt).await {
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
