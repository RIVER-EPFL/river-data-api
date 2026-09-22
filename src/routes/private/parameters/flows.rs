//! The tracked job absorbing one global parameter into another.

use async_trait::async_trait;
use sea_orm::DbErr;

use crate::routes::private::reprocessing_jobs::flows::{merge_origin, required_uuid};
use crate::routes::private::reprocessing_jobs::service::{Job, JobContext, JobReport};

/// Absorb one global parameter into another, re-points every `site_parameter`, reading, status
/// event, and stream from source to target, then deletes the source. Idempotent under the reaper's
/// re-execution after a lost lease; not offered as a rerun.
/// Backs the `merge_parameters` operator action.
pub struct MergeParameters;

#[async_trait]
impl Job for MergeParameters {
    fn name(&self) -> &'static str {
        "merge_parameters"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let req = crate::routes::private::parameters::service::MergeParametersRequest {
            source_parameter_id: required_uuid(ctx.params(), "source_parameter_id")?,
            target_parameter_id: required_uuid(ctx.params(), "target_parameter_id")?,
        };
        let result = crate::routes::private::parameters::service::merge_parameters(
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
        ctx.info(&format!(
            "Merged {} slots and reassigned {}, moving {} readings and {} streams onto the survivor",
            result.sites_merged,
            result.sites_reassigned,
            result.readings_moved,
            result.streams_updated
        ))
        .await;
        ctx.report(
            JobReport::new()
                .scope("source_deleted", result.source_deleted)
                .count("sites_merged", result.sites_merged)
                .count("sites_reassigned", result.sites_reassigned)
                .count("readings_moved", result.readings_moved)
                .count("streams_updated", result.streams_updated),
        )
        .await;
        Ok(i64::try_from(result.readings_moved).unwrap_or(i64::MAX))
    }
}
