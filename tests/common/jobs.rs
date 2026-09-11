//! A `Job` whose body is a closure, for driving the worker pool with test-defined work.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use river_db::routes::private::reprocessing_jobs::service::JobContext;
use river_db::routes::private::reprocessing_jobs::service::{Job, JobRegistry};
use sea_orm::DbErr;

type Work =
    dyn Fn(JobContext) -> Pin<Box<dyn Future<Output = Result<i64, DbErr>> + Send>> + Send + Sync;

pub struct ClosureJob {
    name: &'static str,
    work: Arc<Work>,
}

impl ClosureJob {
    pub fn new<F, Fut>(name: &'static str, work: F) -> Self
    where
        F: Fn(JobContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<i64, DbErr>> + Send + 'static,
    {
        Self {
            name,
            work: Arc::new(move |ctx| Box::pin(work(ctx))),
        }
    }
}

#[async_trait]
impl Job for ClosureJob {
    fn name(&self) -> &'static str {
        self.name
    }
    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        (self.work)(ctx).await
    }
}

/// A registry holding only `job`.
pub fn registry_of(job: ClosureJob) -> JobRegistry {
    let mut registry = JobRegistry::new();
    registry.register(Arc::new(job));
    registry
}

/// How long a test waits for its own job before saying so.
const JOB_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Wait for one tracked job to reach a terminal status, and return it.
///
/// A test may not end while its own writer is still running: the worker is detached, so a job
/// still going when the test returns writes into the next test's database, and its INSERT races
/// that test's cleanup TRUNCATE. Every path that starts a job and then asserts on what it did
/// waits here.
///
/// Fails with the row's own status and `error_message` rather than a bare timeout, because a job
/// that failed and a job that never ran want different fixes.
pub async fn wait_for_job(db: &sea_orm::DatabaseConnection, job_id: &str) -> String {
    use sea_orm::ConnectionTrait;

    let id = uuid::Uuid::parse_str(job_id).expect("a job id");
    let started = std::time::Instant::now();
    loop {
        let row = db
            .query_one_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT status, error_message, retry_count FROM reprocessing_jobs WHERE id = $1",
                [id.into()],
            ))
            .await
            .expect("read the job row")
            .expect("the enqueued job row");
        let status: String = row.try_get("", "status").expect("status");
        if !matches!(
            status.as_str(),
            "queued" | "pending" | "running" | "retrying"
        ) {
            return status;
        }
        assert!(
            started.elapsed() < JOB_WAIT,
            "job {job_id} is still '{status}' after {}s (retry {}, last error {:?})",
            JOB_WAIT.as_secs(),
            row.try_get::<i32>("", "retry_count").unwrap_or_default(),
            row.try_get::<Option<String>>("", "error_message")
                .unwrap_or_default(),
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Wait for the newest job a trigger enqueued, for a test that holds the trigger rather than the
/// job id. `trigger_id` narrows it to one entity's job: an earlier job of the same type would
/// otherwise satisfy the wait, and a create that enqueued nothing would read as success.
pub async fn wait_for_triggered_job(
    db: &sea_orm::DatabaseConnection,
    trigger_type: &str,
    trigger_id: Option<&str>,
) -> String {
    use sea_orm::ConnectionTrait;

    let (predicate, binds): (String, Vec<sea_orm::Value>) = match trigger_id {
        Some(id) => (
            "trigger_type = $1 AND trigger_id = $2::uuid".to_string(),
            vec![trigger_type.into(), id.into()],
        ),
        None => ("trigger_type = $1".to_string(), vec![trigger_type.into()]),
    };
    let sql = format!(
        "SELECT status, error_message FROM reprocessing_jobs WHERE {predicate} \
         ORDER BY created_at DESC LIMIT 1"
    );
    let started = std::time::Instant::now();
    loop {
        let row = db
            .query_one_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                &sql,
                binds.clone(),
            ))
            .await
            .expect("read the job row");
        let status: Option<String> = row
            .as_ref()
            .and_then(|r| r.try_get::<String>("", "status").ok());
        if let Some(status) = status.as_deref()
            && !matches!(status, "queued" | "pending" | "running" | "retrying")
        {
            return status.to_string();
        }
        assert!(
            started.elapsed() < JOB_WAIT,
            "{trigger_type} job for {trigger_id:?} is {status:?} after {}s (last error {:?})",
            JOB_WAIT.as_secs(),
            row.and_then(|r| r
                .try_get::<Option<String>>("", "error_message")
                .unwrap_or_default()),
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
