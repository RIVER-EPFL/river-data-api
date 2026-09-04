//! A `Job` whose body is a closure, for driving the worker pool with test-defined work.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use river_db::routes::private::reprocessing_jobs::job::{Job, JobRegistry};
use river_db::routes::private::reprocessing_jobs::lifecycle::JobContext;
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
