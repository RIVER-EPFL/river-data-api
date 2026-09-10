//! The tracked job a slot's sd-estimator declaration enqueues.

use async_trait::async_trait;
use sea_orm::{ConnectionTrait, DbErr, Statement};

use super::service::SLOT_SCOPE;
use crate::routes::private::reprocessing_jobs::job::Job;
use crate::routes::private::reprocessing_jobs::jobs::uuid_array;
use crate::routes::private::reprocessing_jobs::lifecycle::{JobContext, JobReport};

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
        // reaches its slot by its pairing, and an unpaired stream reaches none.
        let mut binds: Vec<sea_orm::Value> = vec![
            target.clone().into(),
            site_parameter_ids.clone().into(),
            stream_ids.clone().into(),
        ];
        let mut window = String::new();
        if let Some(start) = start {
            let parsed = chrono::DateTime::parse_from_rfc3339(start)
                .map_err(|e| DbErr::Custom(format!("invalid start: {e}")))?;
            binds.push(sea_orm::Value::from(parsed));
            window.push_str(&format!(" AND s.collected_at >= ${}", binds.len()));
        }
        if let Some(end) = end {
            let parsed = chrono::DateTime::parse_from_rfc3339(end)
                .map_err(|e| DbErr::Custom(format!("invalid end: {e}")))?;
            binds.push(sea_orm::Value::from(parsed));
            window.push_str(&format!(" AND s.collected_at <= ${}", binds.len()));
        }
        let instant_guard = if override_instants {
            ""
        } else {
            " AND s.sd_estimator_source <> 'sample'"
        };

        let skipped = if override_instants {
            0
        } else {
            ctx.db()
                .query_one_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    format!(
                        "SELECT COUNT(*)::bigint AS n FROM samples s \
                         WHERE {SLOT_SCOPE} AND s.sd_estimator_source = 'sample' \
                           AND s.sd_estimator IS DISTINCT FROM $1{window}"
                    ),
                    binds.clone(),
                ))
                .await?
                .map_or(Ok(0_i64), |row| row.try_get::<i64>("", "n"))?
        };

        ctx.info(&format!(
            "Setting the sd estimator of the samples in scope to '{target}'"
        ))
        .await;

        // The UPDATE fires the samples trigger per row, which recomputes `stdev` from the
        // replicates under the new divisor. Nothing here writes a statistic.
        let retagged = ctx
            .db()
            .execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "UPDATE samples s SET sd_estimator = $1, sd_estimator_source = 'slot' \
                     WHERE {SLOT_SCOPE} AND s.sd_estimator IS DISTINCT FROM $1{instant_guard}{window}"
                ),
                binds,
            ))
            .await?
            .rows_affected();

        // The samples trigger fires on readings, not on the samples row itself, so the UPDATE
        // above changes the declaration without recomputing. Refresh each touched row explicitly.
        ctx.db()
            .execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT refresh_sample_aggregate(s.id) FROM samples s \
                     WHERE {SLOT_SCOPE} AND s.sd_estimator = $1{instant_guard}"
                ),
                vec![
                    target.clone().into(),
                    site_parameter_ids.clone().into(),
                    stream_ids.clone().into(),
                ],
            ))
            .await?;

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
