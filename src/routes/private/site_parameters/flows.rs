//! The tracked jobs a slot's sd-estimator declaration and a slot merge enqueue.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sea_orm::sea_query::{Alias, Condition, Expr, Func, Query as SeaQuery};
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, PaginatorTrait, QueryFilter};

use super::service::slot_scope;
use crate::routes::private::readings::samples;
use crate::routes::private::reprocessing_jobs::flows::{merge_origin, required_uuid, uuid_array};
use crate::routes::private::reprocessing_jobs::service::{Job, JobContext, JobReport};

/// Bring existing samples into line with a slot's declared sd estimator, then recompute their
/// statistics.
///
/// The estimator only reaches `samples.stdev`; the mean is unchanged and grabs are excluded from
/// the continuous aggregates, so this refreshes no aggregate. Rerunnable: the UPDATE skips rows
/// already at the target.
///
/// A sample whose estimator was chosen for that one instant (`sd_estimator_source = 'sample'`) is
/// left alone. A slot-level declaration is a statement about the parameter, not a licence to
/// overwrite a decision someone made about a single collection group; `override_instants` says
/// otherwise, explicitly.
pub struct SdEstimatorRetag;

#[async_trait]
impl Job for SdEstimatorRetag {
    fn name(&self) -> &'static str {
        "sd_estimator_retag"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let params = ctx.params();
        let target = params
            .get("estimator")
            .and_then(serde_json::Value::as_str)
            .filter(|e| matches!(*e, "sample" | "population"))
            .ok_or_else(|| {
                DbErr::Custom("sd_estimator_retag needs estimator 'sample' or 'population'".into())
            })?
            .to_string();
        let site_parameter_ids = uuid_array(params, "site_parameter_ids");
        let stream_ids = uuid_array(params, "stream_ids");
        if site_parameter_ids.is_empty() && stream_ids.is_empty() {
            return Err(DbErr::Custom(
                "sd_estimator_retag needs site_parameter_ids or stream_ids".into(),
            ));
        }
        let override_instants = params
            .get("override_instants")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let start = params.get("start").and_then(serde_json::Value::as_str);
        let end = params.get("end").and_then(serde_json::Value::as_str);

        // The scope names slots, so it resolves through `site_parameters` either way: a stream
        // reaches its slot by its pairing, and an unpaired stream reaches none. `sd_estimator` is
        // NOT NULL, so `ne` is the `IS DISTINCT FROM` this carried.
        let parse = |bound: Option<&str>, which: &str| -> Result<Option<DateTime<Utc>>, DbErr> {
            bound
                .map(|b| {
                    DateTime::parse_from_rfc3339(b)
                        .map(|t| t.with_timezone(&Utc))
                        .map_err(|e| DbErr::Custom(format!("invalid {which}: {e}")))
                })
                .transpose()
        };
        let start = parse(start, "start")?;
        let end = parse(end, "end")?;

        let mut window = Condition::all();
        if let Some(start) = start {
            window = window.add(samples::Column::CollectedAt.gte(start));
        }
        if let Some(end) = end {
            window = window.add(samples::Column::CollectedAt.lte(end));
        }
        let in_scope = Condition::all()
            .add(slot_scope(&site_parameter_ids, &stream_ids))
            .add(window.clone());
        let instant_guard = |cond: Condition| -> Condition {
            if override_instants {
                cond
            } else {
                cond.add(samples::Column::SdEstimatorSource.ne("sample"))
            }
        };

        let skipped = if override_instants {
            0
        } else {
            samples::Entity::find()
                .filter(in_scope.clone())
                .filter(samples::Column::SdEstimatorSource.eq("sample"))
                .filter(samples::Column::SdEstimator.ne(target.clone()))
                .count(ctx.db())
                .await?
        };

        ctx.info(&format!(
            "Setting the sd estimator of the samples in scope to '{target}'"
        ))
        .await;

        // The UPDATE fires the samples trigger per row, which recomputes `stdev` from the
        // replicates under the new divisor. Nothing here writes a statistic.
        let retagged = samples::Entity::update_many()
            .col_expr(samples::Column::SdEstimator, Expr::value(target.clone()))
            .col_expr(samples::Column::SdEstimatorSource, Expr::value("slot"))
            .filter(instant_guard(
                in_scope
                    .clone()
                    .add(samples::Column::SdEstimator.ne(target.clone())),
            ))
            .exec(ctx.db())
            .await?
            .rows_affected;

        // The samples trigger fires on readings, not on the samples row itself, so the UPDATE
        // above changes the declaration without recomputing. Refresh each touched row explicitly.
        let mut refresh = SeaQuery::select();
        refresh
            .expr(
                Func::cust(Alias::new("refresh_sample_aggregate"))
                    .arg(Expr::col(samples::Column::Id)),
            )
            .from(samples::Entity)
            .cond_where(instant_guard(
                Condition::all()
                    .add(slot_scope(&site_parameter_ids, &stream_ids))
                    .add(samples::Column::SdEstimator.eq(target.clone())),
            ));
        let refresh = ctx.db().get_database_backend().build(&refresh);
        ctx.db().query_all_raw(refresh).await?;

        if skipped > 0 {
            ctx.log(
                "info",
                &format!(
                    "{skipped} sample(s) keep an estimator chosen for that instant; \
                     rerun with override_instants to change them too"
                ),
                serde_json::json!({ "skipped_instant_decisions": skipped }),
            )
            .await;
        }

        if retagged > 0
            && let Some(state) = crate::common::global_app_state()
        {
            state.response_cache.invalidate_all();
        }

        ctx.report(
            JobReport::new()
                .scope(
                    "site_parameter_ids",
                    site_parameter_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>(),
                )
                .scope(
                    "stream_ids",
                    stream_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>(),
                )
                .scope("override_instants", override_instants)
                .scope("estimator", target)
                .count("samples_retagged", retagged)
                .count("instant_decisions_skipped", skipped),
        )
        .await;
        Ok(retagged.try_into().unwrap_or(i64::MAX))
    }
}

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
