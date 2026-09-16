//! The tracked job a slot merge enqueues.

use async_trait::async_trait;
use sea_orm::DbErr;

use crate::routes::private::reprocessing_jobs::flows::{merge_origin, required_uuid};
use crate::routes::private::reprocessing_jobs::service::{Job, JobContext, JobReport};

/// Absorb one `site_parameter` into another, moves readings, status events, streams, and
/// deployments, then deletes the source. Idempotent on the readings PK and a no-op DELETE of an
/// absent source, so it is safe under the reaper's re-execution after a lost lease. Not offered as
/// a rerun. Backs the `merge_site_parameters` operator action.
pub struct MergeSiteParameters;

#[async_trait]
impl Job for MergeSiteParameters {
    fn name(&self) -> &'static str {
        "merge_site_parameters"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let req = crate::routes::private::site_parameters::service::MergeSiteParametersRequest {
            source_site_parameter_id: required_uuid(ctx.params(), "source_site_parameter_id")?,
            target_site_parameter_id: required_uuid(ctx.params(), "target_site_parameter_id")?,
        };
        let result = crate::routes::private::site_parameters::service::merge_site_parameters(
            ctx.db(),
            &req,
            ctx.params()
                .get("actor")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("system"),
            merge_origin(ctx.params()),
        )
        .await
        .map_err(|e| DbErr::Custom(e.to_string()))?;
        ctx.report(
            JobReport::new()
                .scope("source_deleted", result.source_deleted)
                .count("merged_readings", result.merged_readings)
                .count("merged_status_events", result.merged_status_events)
                .count("streams_updated", result.streams_updated)
                .count("deployments_moved", result.deployments_moved),
        )
        .await;
        Ok(i64::try_from(result.merged_readings).unwrap_or(i64::MAX))
    }
}
