//! The single tracked-job lifecycle: insert a `reprocessing_jobs` row, emit SSE events, run the
//! work closure (with retry + backoff), and record the terminal status. Every background job in the
//! API flows through here so that observability, retry, and (eventually) cancellation/rerun are
//! defined once. Domain glue (e.g. `spawn_reprocessing_job` for sensor reprocessing) wraps these.

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

/// Spawn a tracked job using the global retry policy. Inserts a `reprocessing_jobs` row
/// (`status = 'pending'`), emits `JobCreated`, then runs `work` in a background task that flips the
/// row through `running` → `completed`/`failed`, emitting `JobProgress`/`JobCompleted`. Returns the
/// job id immediately. `sensor_id`/`trigger_id` are nullable.
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
    let policy = job_retry_policy();
    spawn_tracked_job_with_retry(
        db,
        sensor_id,
        trigger_type,
        trigger_id,
        events,
        policy.max_retries,
        policy.backoff_base,
        work,
    )
    .await
}

/// Like [`spawn_tracked_job`] but with an explicit retry policy. On a failed attempt the row is set
/// to `retrying` (bumping `retry_count`) and `make_work` is re-invoked after an exponential backoff
/// (`backoff_base * 2^(attempt-1)`); only after `max_retries` exhausted is the job marked `failed`.
/// `make_work` must be `Fn` (callable once per attempt). `sensor_id`/`trigger_id` are nullable.
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

        let mut attempt = 0u32;
        let outcome = loop {
            match make_work(db.clone()).await {
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
