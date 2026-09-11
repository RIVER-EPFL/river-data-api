//! Concrete `Job` implementations: the worker-run handler for each `trigger_type`. Each reads its
//! inputs from `ctx.params()` and calls the same service function the inline trigger used.

use std::time::Duration;

use crate::routes::private::derived_parameters::models::definition as formulas;
use crate::routes::private::derived_parameters::models::source as sources;
use crate::routes::private::readings::models as readings;
use crate::routes::private::site_parameters::models as site_parameters;
use async_trait::async_trait;
use sea_orm::sea_query;
use sea_orm::{ConnectionTrait, DbErr, EntityTrait, FromQueryResult, QuerySelect, Statement};
use uuid::Uuid;

use super::job::{Job, TunableKind, TunableSpec};
use super::lifecycle::{JobContext, JobReport};
use super::schedule::Schedule;
use crate::common::sync_state;
use crate::config::Config;
use crate::routes::private::sensor_calibrations::service::{
    recalculate_derived_at_timestamp, reprocess_sensor_readings, reprocess_site_parameter_readings,
};
use crate::routes::private::sensor_deployments as deployments;

/// One instant of one site's derived work.
#[derive(FromQueryResult)]
struct DerivedInstant {
    site_id: Uuid,
    time: chrono::DateTime<chrono::FixedOffset>,
}

/// One instant, where the site is already known.
#[derive(FromQueryResult)]
struct InstantRow {
    time: chrono::DateTime<chrono::FixedOffset>,
}

/// A stream named by its source pair.
#[derive(FromQueryResult)]
struct StreamRef {
    source_system: String,
    source_key: String,
}

/// `Job::run` answers in `DbErr`, the refresh in `AppError`. A refresh that could not run fails
/// the job that asked for it rather than being logged and forgotten.
pub(crate) fn as_db_err(e: crate::error::AppError) -> DbErr {
    DbErr::Custom(e.to_string())
}

pub(crate) fn required_uuid(params: &serde_json::Value, key: &str) -> Result<Uuid, DbErr> {
    params
        .get(key)
        .and_then(serde_json::Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| DbErr::Custom(format!("job params missing uuid {key}")))
}

/// The origin the request that enqueued a merge was recorded under. A row queued before the origin
/// travelled with the actor names none, and a merge is asked for by an operator, so that reads as
/// manual.
fn merge_origin(params: &serde_json::Value) -> crate::routes::private::readings::models::Origin {
    params
        .get("origin")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or(crate::routes::private::readings::models::Origin::Manual)
}

pub(crate) fn optional_uuid(params: &serde_json::Value, key: &str) -> Option<Uuid> {
    params
        .get(key)
        .and_then(serde_json::Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
}

/// Parse a `params` array of RFC 3339 strings into UTC timestamps (skipping unparseable entries).
fn parse_timestamps(value: Option<&serde_json::Value>) -> Vec<chrono::DateTime<chrono::Utc>> {
    value
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(serde_json::Value::as_str)
                .filter_map(|s| {
                    chrono::DateTime::parse_from_rfc3339(s)
                        .ok()
                        .map(|t| t.with_timezone(&chrono::Utc))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse an array of UUID strings under `key` (missing/empty → empty vec). Non-UUID elements are
/// skipped; the persisted params are produced by our own handlers, so this is defensive only.
pub(crate) fn uuid_array(params: &serde_json::Value, key: &str) -> Vec<Uuid> {
    params
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().and_then(|s| Uuid::parse_str(s).ok()))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse an array of `[site_id, parameter_id]` UUID pairs under `key`. Each element is a two-string
/// array; malformed elements are skipped.
/// What a loop over slots did: how many succeeded, which failed and why, and the total the
/// successful ones moved. A run whose every slot failed is a failed run, not a completed one that
/// happened to move nothing.
pub struct SlotOutcome {
    pub succeeded: usize,
    pub failed: Vec<(serde_json::Value, String)>,
    pub readings: i64,
}

impl SlotOutcome {
    pub fn from(
        results: impl IntoIterator<Item = (serde_json::Value, Result<i64, DbErr>)>,
    ) -> Self {
        let mut outcome = Self {
            succeeded: 0,
            failed: Vec::new(),
            readings: 0,
        };
        for (slot, result) in results {
            match result {
                Ok(n) => {
                    outcome.succeeded += 1;
                    outcome.readings += n;
                }
                Err(e) => outcome.failed.push((slot, e.to_string())),
            }
        }
        outcome
    }

    #[must_use]
    pub fn all_failed(&self) -> bool {
        self.succeeded == 0 && !self.failed.is_empty()
    }

    /// One timeline line per failed slot, then the counts and the failed set on the report.
    pub(crate) async fn record(&self, ctx: &JobContext, report: JobReport) -> JobReport {
        for (slot, error) in &self.failed {
            ctx.log(
                "warn",
                "slot failed",
                serde_json::json!({ "slot": slot, "error": error }),
            )
            .await;
        }
        report
            .scope(
                "failed_slots",
                self.failed
                    .iter()
                    .map(|(slot, _)| slot.clone())
                    .collect::<Vec<_>>(),
            )
            .count("slots_failed", self.failed.len())
    }

    pub(crate) fn error(&self) -> DbErr {
        DbErr::Custom(format!("every one of {} slots failed", self.failed.len()))
    }
}

pub(crate) fn uuid_pair_array(params: &serde_json::Value, key: &str) -> Vec<(Uuid, Uuid)> {
    params
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    let pair = v.as_array()?;
                    let a = pair
                        .first()?
                        .as_str()
                        .and_then(|s| Uuid::parse_str(s).ok())?;
                    let b = pair
                        .get(1)?
                        .as_str()
                        .and_then(|s| Uuid::parse_str(s).ok())?;
                    Some((a, b))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse an optional RFC-3339 timestamp under `key`.
pub(crate) fn optional_datetime(
    params: &serde_json::Value,
    key: &str,
) -> Option<chrono::DateTime<chrono::Utc>> {
    params
        .get(key)
        .and_then(serde_json::Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
}
/// Re-derive FK columns and `calibrated_value` for one sensor's readings. Backs the sensor-scoped
/// reprocess triggers (manual reprocess, calibration changes).
pub struct ReprocessSensor {
    name: &'static str,
}

impl ReprocessSensor {
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self { name }
    }
}

#[async_trait]
impl Job for ReprocessSensor {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        use sea_orm::sea_query::ExprTrait;

        let sensor_id = required_uuid(ctx.params(), "sensor_id")?;
        ctx.info(&format!("Reprocessing readings for sensor {sensor_id}"))
            .await;
        let count = reprocess_sensor_readings(ctx.db(), sensor_id, Some(ctx.job_id())).await?;
        if let Ok(Some(row)) = ctx
            .db()
            .query_one_raw(build(
                &sea_query::Query::select()
                    .distinct()
                    .column(readings::Column::SiteId)
                    .from(readings::Entity)
                    .and_where(sea_query::Expr::col(readings::Column::SensorId).eq(sensor_id))
                    .and_where(sea_query::Expr::col(readings::Column::SiteId).is_not_null())
                    .limit(1)
                    .to_owned(),
            ))
            .await
        {
            if let Ok(site_id) = row.try_get::<Uuid>("", "site_id") {
                ctx.set_site(site_id).await;
            }
        }
        ctx.report(
            JobReport::new()
                .scope("sensor_id", sensor_id.to_string())
                .count("readings_updated", count),
        )
        .await;
        Ok(count as i64)
    }
}

/// Refresh the rollups over the window a caller states, so a change it already committed is
/// visible before the hourly policy would carry it: `from` and `until` for a range, `since` for
/// everything after an instant. A run naming no window rematerialises the whole history, which is
/// the repair for a database edited out of band and the only thing left that the policies and the
/// real-time head do not do by themselves.
pub struct RefreshAggregates;

#[async_trait]
impl Job for RefreshAggregates {
    fn name(&self) -> &'static str {
        "refresh_aggregates"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let instant = |key: &str| {
            ctx.params()
                .get(key)
                .and_then(serde_json::Value::as_str)
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&chrono::Utc))
        };
        let window = match (instant("from"), instant("until"), instant("since")) {
            (Some(from), Some(until), _) => crate::common::aggregates::Window::Range(from, until),
            (_, _, Some(since)) => crate::common::aggregates::Window::Since(since),
            _ => crate::common::aggregates::Window::Full,
        };
        // A refresh that could not run must fail the job: reporting `completed` while the rollups
        // still serve the old numbers is the failure this job exists to make visible.
        let outcome = tokio::time::timeout(
            Duration::from_secs(600),
            crate::common::aggregates::refresh(ctx.db(), window),
        )
        .await;
        match outcome {
            Ok(Ok(())) => {
                ctx.report(JobReport::new().scope("window", format!("{window:?}")))
                    .await;
                Ok(0)
            }
            Ok(Err(e)) => Err(DbErr::Custom(e.to_string())),
            Err(_) => Err(DbErr::Custom(
                "Aggregate refresh timed out after 10 minutes".into(),
            )),
        }
    }
}

/// Re-derive readings for one (site, parameter) slot, and the sensor too when `sensor_id` is given.
/// Backs slot-scoped triggers (stream pairing, sensor swap, adopt).
pub struct ReprocessSlot {
    name: &'static str,
}

impl ReprocessSlot {
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self { name }
    }
}

#[async_trait]
impl Job for ReprocessSlot {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let site_id = required_uuid(ctx.params(), "site_id")?;
        let parameter_id = required_uuid(ctx.params(), "parameter_id")?;
        let count =
            reprocess_site_parameter_readings(ctx.db(), site_id, parameter_id, Some(ctx.job_id()))
                .await? as i64;
        if let Some(sensor_id) = optional_uuid(ctx.params(), "sensor_id") {
            reprocess_sensor_readings(ctx.db(), sensor_id, Some(ctx.job_id())).await?;
        }
        ctx.set_site(site_id).await;
        ctx.report(
            JobReport::new()
                .scope("site_id", site_id.to_string())
                .scope("parameter_id", parameter_id.to_string())
                .count("readings_updated", count),
        )
        .await;
        Ok(count)
    }
}

/// Re-derive readings for a sensor's deployment slot. Derives the slot parameter from the sensor,
/// then re-derives the (site, parameter) slot and the sensor. Backs the deployment-change triggers.
pub struct ReprocessDeployment {
    name: &'static str,
}

impl ReprocessDeployment {
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self { name }
    }
}

#[async_trait]
impl Job for ReprocessDeployment {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let sensor_id = required_uuid(ctx.params(), "sensor_id")?;
        let site_id = required_uuid(ctx.params(), "site_id")?;
        // The deployment's parameter is carried in the job params (spawn_slot_reprocess). Fall back to
        // the sensor's deployment at this site for jobs queued before the parameter was passed through.
        let parameter_id: Option<Uuid> = match ctx
            .params()
            .get("parameter_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
        {
            Some(p) => Some(p),
            None => ctx
                .db()
                .query_one_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT parameter_id FROM sensor_deployments \
                     WHERE sensor_id = $1 AND site_id = $2 \
                     ORDER BY (deployed_until IS NULL) DESC, deployed_from DESC LIMIT 1",
                    [sensor_id.into(), site_id.into()],
                ))
                .await?
                .map(|r| r.try_get::<Option<Uuid>>("", "parameter_id"))
                .transpose()?
                .flatten(),
        };
        let count = if let Some(parameter_id) = parameter_id {
            reprocess_site_parameter_readings(ctx.db(), site_id, parameter_id, Some(ctx.job_id()))
                .await? as i64
        } else {
            0
        };
        reprocess_sensor_readings(ctx.db(), sensor_id, Some(ctx.job_id())).await?;
        ctx.set_site(site_id).await;
        ctx.report(
            JobReport::new()
                .scope("sensor_id", sensor_id.to_string())
                .scope("site_id", site_id.to_string())
                .count("readings_updated", count),
        )
        .await;
        Ok(count)
    }
}

/// Recompute derived values from their source readings, then refresh continuous aggregates. Backs
/// the `derived_recompute` trigger, in either of two scopes: one derived parameter definition over
/// its whole history (`derived_definition_id`), or every definition reading a given slot over a
/// window (`site_ids`, `parameter_ids`, `start`, `end`), which is what a curation decision leaves
/// behind.
pub struct DerivedRecompute;

/// A built query as the statement the connection takes.
fn build(query: &sea_query::SelectStatement) -> Statement {
    let (sql, values) = query.build(sea_query::PostgresQueryBuilder);
    Statement::from_sql_and_values(sea_orm::DatabaseBackend::Postgres, sql, values)
}

/// The `(site, time)` instants a derived recompute covers: every reading of a parameter some
/// calculation reads, at a site whose slot for that calculation's output is tool-entered.
fn derived_instants(
    scope: sea_query::Condition,
    join_definition_on: Option<Uuid>,
) -> sea_query::SelectStatement {
    use sea_orm::sea_query::ExprTrait;

    let r = sea_query::Alias::new("r");
    let d = sea_query::Alias::new("d");
    let dps = sea_query::Alias::new("dps");
    let sp = sea_query::Alias::new("sp");

    let mut query = sea_query::Query::select();
    query
        .distinct()
        .column((r.clone(), readings::Column::SiteId))
        .column((r.clone(), readings::Column::Time))
        .from_as(readings::Entity, r.clone());

    match join_definition_on {
        // One calculation: the definition is the given row, and the sources join to it.
        Some(definition_id) => {
            query
                .join_as(
                    sea_query::JoinType::Join,
                    formulas::Entity,
                    d.clone(),
                    sea_query::Expr::col((d.clone(), formulas::Column::Id)).eq(definition_id),
                )
                .join_as(
                    sea_query::JoinType::Join,
                    sources::Entity,
                    dps.clone(),
                    sea_query::Condition::all()
                        .add(
                            sea_query::Expr::col((
                                dps.clone(),
                                sources::Column::DerivedDefinitionId,
                            ))
                            .equals((d.clone(), formulas::Column::Id)),
                        )
                        .add(
                            sea_query::Expr::col((dps.clone(), sources::Column::ParameterId))
                                .equals((r.clone(), readings::Column::ParameterId)),
                        ),
                );
        }
        // Every calculation that reads the parameter this reading carries.
        None => {
            query
                .join_as(
                    sea_query::JoinType::Join,
                    sources::Entity,
                    dps.clone(),
                    sea_query::Expr::col((dps.clone(), sources::Column::ParameterId))
                        .equals((r.clone(), readings::Column::ParameterId)),
                )
                .join_as(
                    sea_query::JoinType::Join,
                    formulas::Entity,
                    d.clone(),
                    sea_query::Expr::col((d.clone(), formulas::Column::Id))
                        .equals((dps.clone(), sources::Column::DerivedDefinitionId)),
                );
        }
    }

    query
        .join_as(
            sea_query::JoinType::Join,
            site_parameters::Entity,
            sp.clone(),
            sea_query::Condition::all()
                .add(
                    sea_query::Expr::col((sp.clone(), site_parameters::Column::SiteId))
                        .equals((r.clone(), readings::Column::SiteId)),
                )
                .add(
                    sea_query::Expr::col((sp.clone(), site_parameters::Column::EntryMode))
                        .eq("tool"),
                )
                .add(
                    sea_query::Expr::col((sp, site_parameters::Column::ParameterId))
                        .equals((d, formulas::Column::OutputParameterId)),
                ),
        )
        .cond_where(scope)
        .order_by((r.clone(), readings::Column::SiteId), sea_query::Order::Asc)
        .order_by((r, readings::Column::Time), sea_query::Order::Asc)
        .to_owned()
}

/// The instants at one site where a calculation's inputs were recorded.
fn instants_a_calculation_reads(definition_id: Uuid, site_id: Uuid) -> sea_query::SelectStatement {
    use sea_orm::sea_query::ExprTrait;

    let r = sea_query::Alias::new("r");
    let dps = sea_query::Alias::new("dps");
    sea_query::Query::select()
        .distinct()
        .column((r.clone(), readings::Column::Time))
        .from_as(readings::Entity, r.clone())
        .join_as(
            sea_query::JoinType::Join,
            sources::Entity,
            dps.clone(),
            sea_query::Expr::col((dps.clone(), sources::Column::ParameterId))
                .equals((r.clone(), readings::Column::ParameterId)),
        )
        .and_where(
            sea_query::Expr::col((dps, sources::Column::DerivedDefinitionId)).eq(definition_id),
        )
        .and_where(sea_query::Expr::col((r.clone(), readings::Column::SiteId)).eq(site_id))
        .order_by((r, readings::Column::Time), sea_query::Order::Asc)
        .to_owned()
}

/// The `(site, time)` instants a `derived_recompute` run must recompute, in either scope.
fn derived_recompute_instants(params: &serde_json::Value) -> Result<Statement, DbErr> {
    if params.get("derived_definition_id").is_some() {
        let derived_id = required_uuid(params, "derived_definition_id")?;
        return Ok(build(&derived_instants(
            sea_query::Condition::all(),
            Some(derived_id),
        )));
    }

    let uuids = |key: &str| -> Result<Vec<Uuid>, DbErr> {
        params
            .get(key)
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().and_then(|s| Uuid::parse_str(s).ok()))
                    .collect()
            })
            .ok_or_else(|| DbErr::Custom(format!("derived_recompute: missing {key}")))
    };
    let time = |key: &str| -> Result<chrono::DateTime<chrono::Utc>, DbErr> {
        params
            .get(key)
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
            .ok_or_else(|| DbErr::Custom(format!("derived_recompute: missing {key}")))
    };

    use sea_orm::sea_query::ExprTrait;

    let r = sea_query::Alias::new("r");
    let window = sea_query::Condition::all()
        .add(sea_query::Expr::col((r.clone(), readings::Column::SiteId)).is_in(uuids("site_ids")?))
        .add(
            sea_query::Expr::col((r.clone(), readings::Column::ParameterId))
                .is_in(uuids("parameter_ids")?),
        )
        .add(sea_query::Expr::col((r.clone(), readings::Column::Time)).gte(time("start")?))
        .add(sea_query::Expr::col((r, readings::Column::Time)).lte(time("end")?));
    Ok(build(&derived_instants(window, None)))
}

#[async_trait]
impl Job for DerivedRecompute {
    fn name(&self) -> &'static str {
        "derived_recompute"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let instants = derived_recompute_instants(ctx.params())?;
        let work = async {
            tracing::info!(job_id = %ctx.job_id(), "Recomputing derived parameters");
            let rows = ctx.db().query_all_raw(instants).await?;

            let total = i32::try_from(rows.len()).unwrap_or(i32::MAX);
            ctx.set_progress(0, Some(total)).await;

            let mut filled: i32 = 0;
            let mut min_filled: Option<chrono::DateTime<chrono::Utc>> = None;
            let mut filled_sites: std::collections::BTreeSet<Uuid> =
                std::collections::BTreeSet::new();
            for (i, row) in rows.iter().enumerate() {
                if ctx.is_cancelled() {
                    break;
                }
                let instant = DerivedInstant::from_query_result(row, "")?;
                let site_id = instant.site_id;
                let utc_time = instant.time.with_timezone(&chrono::Utc);
                match recalculate_derived_at_timestamp(ctx.db(), site_id, utc_time).await {
                    Ok(()) => {
                        filled += 1;
                        min_filled = Some(min_filled.map_or(utc_time, |m| m.min(utc_time)));
                        filled_sites.insert(site_id);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, time = %utc_time, "Failed to recompute derived value")
                    }
                }
                if (i + 1) % 500 == 0 {
                    ctx.set_progress(i as i32 + 1, Some(total)).await;
                }
            }

            if let Some(since) = min_filled {
                tracing::info!(%since, "Refreshing continuous aggregates after derived recompute");
                sync_state::refresh_continuous_aggregates(ctx.db(), since)
                    .await
                    .map_err(as_db_err)?;
                for site_id in filled_sites {
                    announce_derived_write(&ctx, site_id, filled);
                }
            }
            ctx.set_progress(total, Some(total)).await;
            ctx.report(
                JobReport::new()
                    .scope_opt(
                        "derived_definition_id",
                        ctx.params()
                            .get("derived_definition_id")
                            .and_then(|v| v.as_str().map(str::to_string)),
                    )
                    .scope_opt("earliest_filled", min_filled.map(|t| t.to_rfc3339()))
                    .count("timestamps", total)
                    .count("filled", filled),
            )
            .await;
            tracing::info!(total, filled, "Derived parameter recomputation complete");
            Ok::<i64, DbErr>(i64::from(filled))
        };

        match tokio::time::timeout(Duration::from_secs(600), work).await {
            Ok(res) => res,
            Err(_) => Err(DbErr::Custom("Timed out after 10 minutes".to_string())),
        }
    }
}

/// Backfill derived values for the readings already present at a site when a derived
/// `site_parameter` is assigned, then refresh continuous aggregates. Backs the `derived_assignment`
/// trigger. Reads `derived_definition_id` and `site_id` from params.
pub struct DerivedAssignment;

#[async_trait]
impl Job for DerivedAssignment {
    fn name(&self) -> &'static str {
        "derived_assignment"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let def_id = required_uuid(ctx.params(), "derived_definition_id")?;
        let site_id = required_uuid(ctx.params(), "site_id")?;
        tracing::info!(%def_id, %site_id, "Computing derived values after site assignment");
        ctx.set_site(site_id).await;

        let rows = ctx
            .db()
            .query_all_raw(build(&instants_a_calculation_reads(def_id, site_id)))
            .await?;

        let mut filled = 0i64;
        let mut earliest: Option<chrono::DateTime<chrono::Utc>> = None;
        for row in &rows {
            if ctx.is_cancelled() {
                break;
            }
            let utc = InstantRow::from_query_result(row, "")?
                .time
                .with_timezone(&chrono::Utc);
            if recalculate_derived_at_timestamp(ctx.db(), site_id, utc)
                .await
                .is_ok()
            {
                filled += 1;
                earliest = Some(earliest.map_or(utc, |e| e.min(utc)));
            }
        }

        if let Some(since) = earliest {
            sync_state::refresh_continuous_aggregates(ctx.db(), since)
                .await
                .map_err(as_db_err)?;
            announce_derived_write(&ctx, site_id, i32::try_from(filled).unwrap_or(i32::MAX));
        }

        ctx.report(
            JobReport::new()
                .scope("derived_definition_id", def_id.to_string())
                .scope("site_id", site_id.to_string())
                .scope_opt("earliest_filled", earliest.map(|t| t.to_rfc3339()))
                .count("timestamps", rows.len())
                .count("filled", filled),
        )
        .await;
        tracing::info!(%def_id, %site_id, filled, "Derived assignment backfill completed");
        Ok(filled)
    }
}

/// Compute and upsert derived parameter values for an explicit list of `(site, timestamps)` pairs,
/// then refresh continuous aggregates from the earliest timestamp. Backs `compute_derived` (the
/// operator action) and `batch_derived` (auto-compute after a batch insert). Reads `site_timestamps`
/// (array of `{ site_id, timestamps[] }`) from params.
pub struct SiteTimestampsDerived {
    name: &'static str,
}

impl SiteTimestampsDerived {
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self { name }
    }
}

// ── Recurring Services (Wave 2) ──────────────────────────────────────────────────────────────────
//
// The background loops formerly spawned in `main.rs` are now `Job` impls the DB-backed scheduler
// enqueues on cadence (so exactly one replica fires each tick). Each `run` calls the SAME loop body
// the periodic task called, once. Each `default_schedule` returns the cadence from `Config` so the
// scheduler can seed a `schedules` row on first start; the seconds are captured at registry-build
// time. Services that need config / shared in-process services beyond `db`+`params` read the
// process-global `AppState` (`crate::common::global_app_state`), the same set-once handle pattern
// the CrudCrate hooks use for the event sender.

/// Fill missing derived readings, refresh continuous aggregates, and prune old tracked-job rows,
/// the derived-consistency janitor. Wraps [`janitor::run_once`] plus the per-tick full/incremental
/// refresh and periodic retention the old `janitor::periodic` loop did.
pub struct JanitorRun {
    /// Fallback cadence for the full-refresh decision, used only when the run carries no
    /// scheduler-stamped `interval_seconds` (`run_now`). The `schedules` row is the authority.
    interval_seconds: u64,
    full_refresh_seconds: u64,
    maintenance_retention_days: u32,
    operator_retention_days: u32,
    maintenance_max_rows: u64,
}

impl JanitorRun {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let retention = crate::common::retention::Retention::from_config(config);
        Self {
            interval_seconds: config.janitor_interval_seconds,
            full_refresh_seconds: config.janitor_full_refresh_seconds,
            maintenance_retention_days: retention.job_maintenance.horizon_days().unwrap_or(0),
            operator_retention_days: retention.job_operator.horizon_days().unwrap_or(0),
            maintenance_max_rows: retention.job_maintenance_max_rows,
        }
    }
}

#[async_trait]
impl Job for SiteTimestampsDerived {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let groups = ctx
            .params()
            .get("site_timestamps")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut work: Vec<(Uuid, Vec<chrono::DateTime<chrono::Utc>>)> = Vec::new();
        for group in &groups {
            let Some(site_id) = optional_uuid(group, "site_id") else {
                continue;
            };
            work.push((site_id, parse_timestamps(group.get("timestamps"))));
        }

        let total =
            i32::try_from(work.iter().map(|(_, ts)| ts.len()).sum::<usize>()).unwrap_or(i32::MAX);
        ctx.set_progress(0, Some(total)).await;

        let mut progress = 0i32;
        let mut earliest: Option<chrono::DateTime<chrono::Utc>> = None;
        'outer: for (site_id, timestamps) in &work {
            for time in timestamps {
                if ctx.is_cancelled() {
                    break 'outer;
                }
                if let Err(e) = recalculate_derived_at_timestamp(ctx.db(), *site_id, *time).await {
                    tracing::warn!(error = %e, site_id = %site_id, time = %time, "Failed to compute derived values");
                } else {
                    earliest = Some(earliest.map_or(*time, |e| e.min(*time)));
                }
                progress += 1;
                if progress % 500 == 0 {
                    ctx.set_progress(progress, Some(total)).await;
                }
            }
        }

        if let Some(since) = earliest {
            tracing::info!(%since, "Refreshing continuous aggregates after derived computation");
            sync_state::refresh_continuous_aggregates(ctx.db(), since)
                .await
                .map_err(as_db_err)?;
            for (site_id, timestamps) in &work {
                announce_derived_write(
                    &ctx,
                    *site_id,
                    i32::try_from(timestamps.len()).unwrap_or(i32::MAX),
                );
            }
        }
        ctx.set_progress(progress, Some(total)).await;
        ctx.report(
            JobReport::new()
                .scope("sites", work.len())
                .scope_opt("earliest_computed", earliest.map(|t| t.to_rfc3339()))
                .count("timestamps", total)
                .count("computed", progress),
        )
        .await;
        tracing::info!(computed = progress, "Derived computation complete");
        Ok(i64::from(progress))
    }
}

/// A derived value is a served value, so a job that writes one announces it: `DataIngested` naming
/// the site is what drops that site's cached responses (`common/cache.rs:17-18`).
fn announce_derived_write(ctx: &JobContext, site_id: Uuid, count: i32) {
    let _ = ctx.events().send(crate::common::AppEvent::DataIngested {
        site_id: Some(site_id),
        parameter_id: None,
        stream_id: None,
        count: usize::try_from(count).unwrap_or(0),
    });
}

/// Auto-compute derived values for one site's newly ingested timestamps. Backs the `ingest_derived`
/// trigger fired after a single-stream ingest. Reads `site_id`, `stream_id`, and `timestamps[]` from
/// params.
pub struct IngestDerived;

#[async_trait]
impl Job for IngestDerived {
    fn name(&self) -> &'static str {
        "ingest_derived"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let site_id = required_uuid(ctx.params(), "site_id")?;
        let stream_id = optional_uuid(ctx.params(), "stream_id");
        let timestamps = parse_timestamps(ctx.params().get("timestamps"));
        let total = i32::try_from(timestamps.len()).unwrap_or(i32::MAX);

        ctx.set_site(site_id).await;
        ctx.report(
            JobReport::new()
                .scope("site_id", site_id.to_string())
                .scope_opt("stream_id", stream_id.map(|id| id.to_string()))
                .count("timestamps", total),
        )
        .await;
        ctx.set_progress(0, Some(total)).await;

        let mut progress = 0i32;
        let mut earliest: Option<chrono::DateTime<chrono::Utc>> = None;
        for time in timestamps {
            if ctx.is_cancelled() {
                break;
            }
            if let Err(e) = recalculate_derived_at_timestamp(ctx.db(), site_id, time).await {
                tracing::warn!(error = %e, site_id = %site_id, time = %time, "Failed to auto-compute derived values after ingest");
            } else {
                earliest =
                    Some(earliest.map_or(time, |e: chrono::DateTime<chrono::Utc>| e.min(time)));
            }
            progress += 1;
            if progress % 500 == 0 {
                ctx.set_progress(progress, Some(total)).await;
            }
        }

        if let Some(since) = earliest {
            sync_state::refresh_continuous_aggregates(ctx.db(), since)
                .await
                .map_err(as_db_err)?;
            announce_derived_write(&ctx, site_id, progress);
        }
        ctx.set_progress(progress, Some(total)).await;
        Ok(i64::from(progress))
    }
}

/// `reprocess_all` operator action. A failed slot logs and continues, a partial backdate is more
/// useful than aborting the whole batch on one bad slot.
pub struct ReprocessAll;

#[async_trait]
impl Job for ReprocessAll {
    fn name(&self) -> &'static str {
        "reprocess_all"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let slots: Vec<(Uuid, Uuid)> = deployments::models::Entity::find()
            .select_only()
            .column(deployments::models::Column::SiteId)
            .column(deployments::models::Column::ParameterId)
            .distinct()
            .into_tuple()
            .all(ctx.db())
            .await?;
        let slot_count = slots.len();
        ctx.info(&format!("Backdating {slot_count} slot(s)")).await;

        let mut results = Vec::with_capacity(slot_count);
        for (site_id, parameter_id) in slots {
            let moved = reprocess_site_parameter_readings(
                ctx.db(),
                site_id,
                parameter_id,
                Some(ctx.job_id()),
            )
            .await
            .map(|n| n as i64);
            results.push((
                serde_json::json!({ "site_id": site_id, "parameter_id": parameter_id }),
                moved,
            ));
        }
        let outcome = SlotOutcome::from(results);
        let total = outcome.readings;
        let report = outcome
            .record(
                &ctx,
                JobReport::new()
                    .count("slots", slot_count)
                    .count("readings_updated", total),
            )
            .await;
        ctx.report(report).await;
        if outcome.all_failed() {
            return Err(outcome.error());
        }
        tracing::info!(readings_updated = total, "reprocess_all complete");
        Ok(total)
    }
}

/// Window-reprocess the slots whose open deployments the handler just backdated, so the
/// previously-unattributed readings are stamped with `sensor_id`/`deployment_id`/`calibration_id`.
/// The slot set is carried in `params.slots` (the handler owns the pre-mutation that picked them).
/// Backs the `backfill_attribution` operator action. A failed slot logs and continues.
pub struct BackfillAttribution;

#[async_trait]
impl Job for BackfillAttribution {
    fn name(&self) -> &'static str {
        "backfill_attribution"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let slots = uuid_pair_array(ctx.params(), "slots");
        let mut results = Vec::with_capacity(slots.len());
        for (site_id, parameter_id) in slots {
            let moved = reprocess_site_parameter_readings(
                ctx.db(),
                site_id,
                parameter_id,
                Some(ctx.job_id()),
            )
            .await
            .map(|n| n as i64);
            results.push((
                serde_json::json!({ "site_id": site_id, "parameter_id": parameter_id }),
                moved,
            ));
        }
        let outcome = SlotOutcome::from(results);
        let total = outcome.readings;
        let report = outcome
            .record(&ctx, JobReport::new().count("readings_updated", total))
            .await;
        ctx.report(report).await;
        if outcome.all_failed() {
            return Err(outcome.error());
        }
        tracing::info!(readings_updated = total, "backfill_attribution complete");
        Ok(total)
    }
}

/// Re-derive `calibrated_value`/`calibration_id` for the sensors carrying readings a calibration
/// window covers but never stamped. The sensor set is carried in `params.sensors`. Backs the
/// `backfill_calibrations` operator action. A failed sensor logs and continues.
pub struct BackfillCalibrations;

#[async_trait]
impl Job for BackfillCalibrations {
    fn name(&self) -> &'static str {
        "backfill_calibrations"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let sensors = uuid_array(ctx.params(), "sensors");
        let mut results = Vec::with_capacity(sensors.len());
        for sensor_id in sensors {
            let moved = reprocess_sensor_readings(ctx.db(), sensor_id, Some(ctx.job_id()))
                .await
                .map(|n| n as i64);
            results.push((serde_json::json!({ "sensor_id": sensor_id }), moved));
        }
        let outcome = SlotOutcome::from(results);
        let total = outcome.readings;
        let report = outcome
            .record(&ctx, JobReport::new().count("readings_updated", total))
            .await;
        ctx.report(report).await;
        if outcome.all_failed() {
            return Err(outcome.error());
        }
        tracing::info!(readings_updated = total, "backfill_calibrations complete");
        Ok(total)
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
        let req = crate::routes::private::admin::merge_services::MergeSiteParametersRequest {
            source_site_parameter_id: required_uuid(ctx.params(), "source_site_parameter_id")?,
            target_site_parameter_id: required_uuid(ctx.params(), "target_site_parameter_id")?,
        };
        let result = crate::routes::private::admin::merge_services::merge_site_parameters(
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
        let req = crate::routes::private::admin::merge_services::MergeParametersRequest {
            source_parameter_id: required_uuid(ctx.params(), "source_parameter_id")?,
            target_parameter_id: required_uuid(ctx.params(), "target_parameter_id")?,
        };
        let result = crate::routes::private::admin::merge_services::merge_parameters(
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
                .count("sites_merged", result.sites_merged)
                .count("sites_reassigned", result.sites_reassigned)
                .count("readings_moved", result.readings_moved)
                .count("streams_updated", result.streams_updated),
        )
        .await;
        Ok(i64::try_from(result.readings_moved).unwrap_or(i64::MAX))
    }
}

/// The status a guarded plan job's work leaves behind, read before it runs.
///
/// A lease lost after the run committed is reclaimed by the reaper and the job runs again. The
/// guard inside `apply_plan`/`revert_plan` then refuses the plan for being past its starting
/// status, which the worker records as a failure over work that in fact succeeded, so the replay
/// is recognised here and reported instead.
async fn plan_status<C: ConnectionTrait>(db: &C, plan_id: Uuid) -> Result<Option<String>, DbErr> {
    Ok(
        crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(plan_id)
            .one(db)
            .await?
            .map(|p| p.status),
    )
}

/// Apply a pairing plan: resolve entities, execute pairings, backfill readings, mark the plan
/// `applied`. The status transition is guarded (only a `draft` plan applies), and a re-execution
/// after a lost lease finds the plan already applied and reports a replay; not offered as a rerun. Backs the `apply_pairing_plan`
/// operator action.
pub struct PlanApply;

#[async_trait]
impl Job for PlanApply {
    fn name(&self) -> &'static str {
        "plan_apply"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let plan_id = required_uuid(ctx.params(), "plan_id")?;
        if plan_status(ctx.db(), plan_id).await?.as_deref() == Some("applied") {
            ctx.report(JobReport::new().scope("plan_id", plan_id.to_string()))
                .await;
            ctx.info("Plan is already applied; this run is a replay and changed nothing")
                .await;
            return Ok(0);
        }
        let result =
            crate::routes::private::sync::service::apply_plan(ctx.db(), plan_id, Some(&ctx))
                .await
                .map_err(|e| DbErr::Custom(e.to_string()))?;
        // Every counter the apply produced, so a reader of the run knows what it created as well
        // as what it paired; the plan's own `apply_result` records the same nine numbers.
        ctx.report(
            JobReport::new()
                .scope("plan_id", plan_id.to_string())
                .count("projects_created", result.projects_created)
                .count("sites_created", result.sites_created)
                .count("parameters_created", result.parameters_created)
                .count("site_parameters_created", result.site_parameters_created)
                .count("streams_paired", result.streams_paired)
                .count("streams_skipped", result.streams_skipped)
                .count("instruments_created", result.instruments_created)
                .count("curves_assigned", result.curves_assigned)
                .count("readings_backfilled", result.readings_backfilled),
        )
        .await;
        ctx.info(&format!(
            "Applied plan: {} streams paired, {} readings backfilled",
            result.streams_paired, result.readings_backfilled
        ))
        .await;
        Ok(i64::try_from(result.readings_backfilled).unwrap_or(i64::MAX))
    }
}

/// Revert an applied pairing plan: unpair every stream it touched, restoring the prior state, and
/// mark the plan `reverted`. The status transition is guarded (only an `applied` plan reverts), and
/// a re-execution after a lost lease finds the plan already reverted and reports a replay; not
/// offered as a rerun. Backs the
/// `revert_pairing_plan` operator action.
pub struct PlanRevert;

#[async_trait]
impl Job for PlanRevert {
    fn name(&self) -> &'static str {
        "plan_revert"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let plan_id = required_uuid(ctx.params(), "plan_id")?;
        if plan_status(ctx.db(), plan_id).await?.as_deref() == Some("reverted") {
            ctx.report(JobReport::new().scope("plan_id", plan_id.to_string()))
                .await;
            ctx.info("Plan is already reverted; this run is a replay and changed nothing")
                .await;
            return Ok(0);
        }
        let reverted = crate::routes::private::sync::service::revert_plan(ctx.db(), plan_id)
            .await
            .map_err(|e| DbErr::Custom(e.to_string()))?;
        ctx.report(
            JobReport::new()
                .scope("plan_id", plan_id.to_string())
                .count("reverted", reverted),
        )
        .await;
        ctx.info(&format!("Reverted plan: {reverted} streams unpaired"))
            .await;
        Ok(i64::from(reverted))
    }
}

#[async_trait]
impl Job for JanitorRun {
    fn name(&self) -> &'static str {
        "janitor_service"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(self.interval_seconds.max(1) as i64))
    }

    // The one concrete tunable: `retention_days` overrides the operator-retention window for the
    // tracked-job prune. Other Services keep the default accept-anything `validate` (no tunables yet)
    // and follow this same pattern when they grow one.
    fn tunables(&self) -> Vec<TunableSpec> {
        vec![TunableSpec {
            key: "retention_days".to_string(),
            kind: TunableKind::Integer,
            min: Some(1),
            max: None,
            default: serde_json::json!(self.operator_retention_days),
            help: "How long an operator or metadata job row is kept before the prune removes it."
                .to_string(),
        }]
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        use crate::routes::private::derived_parameters::flows as janitor;
        let db = ctx.db();

        // A scheduled run carries the schedule's tunables snapshot under `params.tunables`
        // (see `scheduler::enqueue_due`); an on-demand `run_now` carries the same key. Fall back to
        // the config-derived default when absent or out of range.
        let operator_retention_days = ctx
            .params()
            .get("tunables")
            .and_then(|t| t.get("retention_days"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n >= 1)
            .unwrap_or(self.operator_retention_days);

        // The scheduled slot and the cadence it fired on, stamped by the scheduler; a `run_now`
        // carries neither and falls back to the wall clock and the configured interval. Both the
        // gap scan's window and the full-refresh period below are decided from them.
        let scheduled_epoch = ctx
            .params()
            .get("scheduled_at")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map_or_else(|| chrono::Utc::now().timestamp(), |t| t.timestamp())
            .max(0) as u64;
        let cadence_seconds = ctx
            .params()
            .get("interval_seconds")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(self.interval_seconds)
            .max(1);
        let do_full = if self.full_refresh_seconds == 0 {
            false
        } else {
            (scheduled_epoch % self.full_refresh_seconds) < cadence_seconds
        };

        // 1. Fill derived gaps, reporting into this job and refreshing aggregates back to the
        //    earliest filled timestamp. Scoped to twice the cadence, so an hourly tick probes an
        //    index range instead of hashing the whole hypertable; each `full_refresh_seconds`
        //    period one tick runs it unbounded, which is what covers drift older than that
        //    window. The tick that carries it is the one whose scheduled slot falls in the first
        //    cadence window of the period, and the cadence is the `schedules` row's, not the
        //    process's: the scheduler stamps both the slot and the interval it fired on into the
        //    job params, so an operator cadence change cannot leave the unbounded scan
        //    unreachable. A `run_now` carries neither and falls back to the wall clock and the
        //    configured interval.
        let since = (!do_full)
            .then(|| chrono::Utc::now() - chrono::Duration::seconds((cadence_seconds * 2) as i64));
        janitor::run_once(db, Some(&ctx), since).await?;

        // 2. Repair corrected readings whose stored value is no longer what their own curves
        //    produce, whichever route moved them apart. Hooks make that repair immediate; this makes
        //    it eventual, so a hook that never fired costs staleness rather than a wrong number.
        //    Refreshed over the span it moved, before the rollups below settle for this tick.
        let mut recomposed = 0u64;
        match crate::routes::private::sensor_calibrations::service::sweep_curve_drift(
            db,
            Some(ctx.job_id()),
        )
        .await
        {
            Ok(drift) if drift.moved > 0 => {
                recomposed = drift.moved;
                tracing::info!(
                    moved = drift.moved,
                    "Janitor: recomposed drifted curve values"
                );
                ctx.log(
                    "info",
                    &format!(
                        "recomposed {} readings whose value had drifted from their curves",
                        drift.moved
                    ),
                    serde_json::json!({}),
                )
                .await;
                if let Some((lo, hi)) = drift.span
                    && let Err(e) = crate::common::aggregates::refresh(
                        db,
                        crate::common::aggregates::Window::Range(lo, hi),
                    )
                    .await
                {
                    tracing::warn!(error = %e, "Janitor: refresh after curve drift failed");
                }
                // A rewritten spot value is an input somebody's calculation read, so the visits it
                // moved recompute in dependency order rather than being left stale (Q108).
                match crate::routes::private::collection_events::flows::events_from_pairs(
                    db,
                    &drift.touched,
                )
                .await
                {
                    Ok(events) => {
                        if let Err(e) =
                            crate::routes::private::collection_events::flows::enqueue_for(
                                db,
                                &events,
                                "janitor",
                                crate::routes::private::collection_events::flows::Writer::Person,
                            )
                            .await
                        {
                            tracing::warn!(error = %e, "Janitor: recompute after curve drift failed");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Janitor: resolving drifted visits failed");
                    }
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "Janitor: curve drift sweep failed"),
        }

        // 3. Tiered tracked-job retention (cheap deletes; idempotent to run every tick).
        let pruned = janitor::prune_tracked_jobs(
            db,
            self.maintenance_retention_days,
            operator_retention_days,
            self.maintenance_max_rows,
        )
        .await;

        // What this tick actually changed, so a run's effect is readable per job rather than only
        // in its logs.
        ctx.report(
            JobReport::new()
                .scope("full_scan", do_full)
                .count("recomposed", recomposed)
                .count("pruned", pruned),
        )
        .await;
        Ok(pruned as i64)
    }
}

/// Retag readings.measurement_type for a sensor/stream scope, then refresh continuous aggregates
/// over the affected window. Backs the bulk reclassification actions (mark sensors low/high
/// frequency, classify sensorless streams): the classification columns (`sensors.data_frequency`,
/// `data_streams.measurement_type`) are updated synchronously by the endpoint; this job makes the
/// existing rows agree. Rerunnable (idempotent, the UPDATE skips rows already at the target).
/// Decompression-safe: portal/lab history lives in compressed (>30-day) chunks.
pub struct MeasurementRetag;

/// The rewrite a `measurement_retag` run makes. `target` is the classification every reading in
/// scope takes; `None` is the 'declared' arm, which joins each reading to its stream and takes the
/// stream's own. The scope also matches by stream ownership: a reading ingested before attribution
/// backfill carries `sensor_id` NULL and belongs to the sensor's streams all the same.
fn retag_readings(
    target: Option<&str>,
    sensor_ids: &[Uuid],
    stream_ids: &[Uuid],
    source_system: Option<&str>,
) -> sea_query::UpdateStatement {
    use crate::routes::private::data_streams::models as data_streams;
    use crate::routes::private::readings::models as readings;
    use sea_orm::sea_query::ExprTrait;

    let r = sea_query::Alias::new("readings");
    let ds = sea_query::Alias::new("data_streams");
    let col = |alias: &sea_query::Alias, column: readings::Column| {
        sea_query::Expr::col((alias.clone(), column))
    };

    let streams_of = |predicate: sea_query::Expr| {
        sea_query::Query::select()
            .column(data_streams::Column::Id)
            .from(data_streams::Entity)
            .and_where(predicate)
            .to_owned()
    };
    let mut scope = sea_query::Condition::any()
        .add(col(&r, readings::Column::SensorId).is_in(sensor_ids.to_vec()))
        .add(col(&r, readings::Column::StreamId).is_in(stream_ids.to_vec()))
        .add(col(&r, readings::Column::StreamId).in_subquery(streams_of(
            sea_query::Expr::col(data_streams::Column::SensorId).is_in(sensor_ids.to_vec()),
        )));
    if let Some(system) = source_system {
        scope = scope.add(col(&r, readings::Column::StreamId).in_subquery(streams_of(
            sea_query::Expr::col(data_streams::Column::SourceSystem).eq(system),
        )));
    }

    let mut update = sea_query::Query::update();
    update.table(readings::Entity);
    match target {
        Some(value) => {
            update
                .value(readings::Column::MeasurementType, value)
                // sea-query has no IS DISTINCT FROM, and a NULL measurement_type reads as
                // continuous, so the comparison cannot be a plain inequality.
                .and_where(sea_query::Expr::cust(format!(
                    r#""readings"."measurement_type" IS DISTINCT FROM '{value}'"#
                )));
        }
        None => {
            update
                .value(
                    readings::Column::MeasurementType,
                    sea_query::Expr::col((ds.clone(), data_streams::Column::MeasurementType)),
                )
                .from(data_streams::Entity)
                .and_where(
                    col(&r, readings::Column::StreamId)
                        .equals((ds.clone(), data_streams::Column::Id)),
                )
                .and_where(
                    sea_query::Expr::col((ds.clone(), data_streams::Column::MeasurementType))
                        .is_not_null(),
                )
                .and_where(sea_query::Expr::cust(
                    r#""readings"."measurement_type" IS DISTINCT FROM "data_streams"."measurement_type""#,
                ));
        }
    }
    update.cond_where(scope);
    update.to_owned()
}

#[async_trait]
impl Job for MeasurementRetag {
    fn name(&self) -> &'static str {
        "measurement_retag"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let params = ctx.params();
        let target = params
            .get("target")
            .and_then(serde_json::Value::as_str)
            .filter(|t| {
                crate::routes::private::readings::service::retag_target_rejection(t).is_none()
            })
            .ok_or_else(|| DbErr::Custom("measurement_retag needs target".to_string()))?
            .to_string();
        // 'declared' aligns each reading with its own stream's classification, for source systems
        // that mix grab and logger columns.
        let declared = target == crate::routes::private::readings::service::RETAG_DECLARED;
        let sensor_ids = uuid_array(params, "sensor_ids");
        let stream_ids = uuid_array(params, "stream_ids");
        let source_system = params
            .get("source_system")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        if sensor_ids.is_empty() && stream_ids.is_empty() && source_system.is_none() {
            return Err(DbErr::Custom(
                "measurement_retag needs sensor_ids, stream_ids, or source_system".to_string(),
            ));
        }

        // The family guard holds here, not only on the HTTP routes: a stored job row is replayed
        // by rerun with its params verbatim, so a route-only guard is bypassed by replaying a row
        // that predates it. 'spot' is what a family already is, and 'declared' realigns readings
        // with the stream's own declaration, which every write path holds at 'spot'.
        if matches!(target.as_str(), "continuous" | "derived") {
            let families =
                crate::routes::private::data_streams::service::family_keys_in_retag_scope(
                    ctx.db(),
                    &sensor_ids,
                    &stream_ids,
                    source_system.as_deref(),
                )
                .await
                .map_err(|e| DbErr::Custom(e.to_string()))?;
            crate::routes::private::data_streams::service::refuse_family_retag(&families, &target)
                .map_err(|e| DbErr::Custom(e.to_string()))?;
        }

        // A stream declaring a different classification will keep writing its own value on
        // ingest, so the retag would drift back; surface the conflict in the job timeline.
        if !declared {
            let conflicting = ctx
                .db()
                .query_all_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT source_system, source_key FROM data_streams \
                     WHERE measurement_type IS NOT NULL AND measurement_type <> $1 \
                       AND (sensor_id = ANY($2) OR id = ANY($3) \
                            OR ($4::text IS NOT NULL AND source_system = $4))",
                    [
                        target.clone().into(),
                        sensor_ids.clone().into(),
                        stream_ids.clone().into(),
                        source_system.clone().into(),
                    ],
                ))
                .await?;
            for row in &conflicting {
                let StreamRef {
                    source_system: system,
                    source_key: key,
                } = StreamRef::from_query_result(row, "")?;
                ctx.log(
                    "warn",
                    &format!(
                        "Stream {system}/{key} declares a different measurement_type; future ingest will keep writing its declared value. Retag the stream too or use target 'declared'."
                    ),
                    serde_json::json!({}),
                )
                .await;
            }
        }

        ctx.info(&format!("Retagging readings in scope to '{target}'"))
            .await;
        let touched = crate::common::bulk_write::guarded_mutation(
            ctx.db(),
            retag_readings(
                declared
                    .then_some(())
                    .map_or(Some(target.as_str()), |()| None),
                &sensor_ids,
                &stream_ids,
                source_system.as_deref(),
            ),
        )
        .await
        .map_err(|e| DbErr::Custom(e.to_string()))?;
        let retagged = touched.rows;

        // Membership in the rollups changed (spot and derived are excluded), so every aggregate is
        // refreshed over what the rewrite touched. A failure here leaves the rollups holding the
        // old membership, so it fails the job rather than being logged.
        let Some((lo, hi)) = touched.span() else {
            ctx.info("Nothing to retag, every reading in scope already matches")
                .await;
            return Ok(0);
        };
        crate::common::aggregates::refresh(
            ctx.db(),
            crate::common::aggregates::Window::Range(lo, hi),
        )
        .await
        .map_err(|e| DbErr::Custom(e.to_string()))?;

        // Reclassified rows change what bounded cached responses would serve.
        if retagged > 0
            && let Some(state) = crate::common::global_app_state()
        {
            state.response_cache.invalidate_all();
        }

        ctx.report(
            JobReport::new()
                .scope("target", target)
                .scope("from", lo.to_rfc3339())
                .scope("until", hi.to_rfc3339())
                .count("readings_retagged", retagged),
        )
        .await;
        Ok(retagged.try_into().unwrap_or(i64::MAX))
    }
}

#[cfg(test)]
#[path = "tests/tunable_validation.rs"]
mod tunable_validation_tests;

#[cfg(test)]
#[path = "tests/slot_outcome.rs"]
mod slot_outcome_tests;

#[cfg(test)]
#[path = "tests/retag_and_instants.rs"]
mod retag_and_instants_tests;
