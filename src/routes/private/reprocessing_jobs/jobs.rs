//! Concrete `Job` implementations — the worker-run handler for each `trigger_type`. Each reads its
//! inputs from `ctx.params()` and calls the same service function the inline trigger used.

use std::time::Duration;

use async_trait::async_trait;
use sea_orm::{ConnectionTrait, DbErr, Statement};
use uuid::Uuid;

use super::job::Job;
use super::lifecycle::JobContext;
use crate::common::sync_state;
use crate::routes::private::sensor_calibrations::services::{
    recalculate_derived_at_timestamp, reprocess_sensor_readings, reprocess_site_parameter_readings,
};

fn required_uuid(params: &serde_json::Value, key: &str) -> Result<Uuid, DbErr> {
    params
        .get(key)
        .and_then(serde_json::Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| DbErr::Custom(format!("job params missing uuid {key}")))
}

fn optional_uuid(params: &serde_json::Value, key: &str) -> Option<Uuid> {
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
/// skipped — the persisted params are produced by our own handlers, so this is defensive only.
fn uuid_array(params: &serde_json::Value, key: &str) -> Vec<Uuid> {
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
fn uuid_pair_array(params: &serde_json::Value, key: &str) -> Vec<(Uuid, Uuid)> {
    params
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    let pair = v.as_array()?;
                    let a = pair.first()?.as_str().and_then(|s| Uuid::parse_str(s).ok())?;
                    let b = pair.get(1)?.as_str().and_then(|s| Uuid::parse_str(s).ok())?;
                    Some((a, b))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse an optional RFC-3339 timestamp under `key`.
fn optional_datetime(
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
        let sensor_id = required_uuid(ctx.params(), "sensor_id")?;
        ctx.info(&format!("Reprocessing readings for sensor {sensor_id}")).await;
        let count = reprocess_sensor_readings(ctx.db(), sensor_id).await?;
        if let Ok(Some(row)) = ctx
            .db()
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT DISTINCT site_id FROM readings WHERE sensor_id = $1 AND site_id IS NOT NULL LIMIT 1",
                [sensor_id.into()],
            ))
            .await
        {
            if let Ok(site_id) = row.try_get::<Uuid>("", "site_id") {
                ctx.set_site(site_id).await;
            }
        }
        ctx.set_detail(serde_json::json!({
            "scope": { "sensor_id": sensor_id },
            "counts": { "readings_updated": count },
        }))
        .await;
        Ok(count as i64)
    }
}

/// Refresh continuous aggregates — incremental (recent window) or full. Single bounded statement.
pub struct RefreshAggregates {
    name: &'static str,
    full: bool,
}

impl RefreshAggregates {
    #[must_use]
    pub fn incremental() -> Self {
        Self { name: "refresh_aggregates", full: false }
    }

    #[must_use]
    pub fn full() -> Self {
        Self { name: "refresh_aggregates_full", full: true }
    }
}

#[async_trait]
impl Job for RefreshAggregates {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let outcome = tokio::time::timeout(Duration::from_secs(600), async {
            if self.full {
                sync_state::refresh_continuous_aggregates_full(ctx.db()).await;
            } else {
                sync_state::refresh_continuous_aggregates(ctx.db(), None).await;
            }
        })
        .await;
        outcome
            .map(|()| 0)
            .map_err(|_| DbErr::Custom("Aggregate refresh timed out after 10 minutes".into()))
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
        let count = reprocess_site_parameter_readings(ctx.db(), site_id, parameter_id).await? as i64;
        if let Some(sensor_id) = optional_uuid(ctx.params(), "sensor_id") {
            reprocess_sensor_readings(ctx.db(), sensor_id).await?;
        }
        ctx.set_site(site_id).await;
        ctx.set_detail(serde_json::json!({
            "scope": { "site_id": site_id, "parameter_id": parameter_id },
            "counts": { "readings_updated": count },
        }))
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
        let parameter_id: Option<Uuid> = ctx
            .db()
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT parameter_id FROM sensors WHERE id = $1",
                [sensor_id.into()],
            ))
            .await?
            .and_then(|r| r.try_get::<Uuid>("", "parameter_id").ok());
        let count = if let Some(parameter_id) = parameter_id {
            reprocess_site_parameter_readings(ctx.db(), site_id, parameter_id).await? as i64
        } else {
            0
        };
        reprocess_sensor_readings(ctx.db(), sensor_id).await?;
        ctx.set_site(site_id).await;
        ctx.set_detail(serde_json::json!({
            "scope": { "sensor_id": sensor_id, "site_id": site_id },
            "counts": { "readings_updated": count },
        }))
        .await;
        Ok(count)
    }
}

/// Recompute every derived value for one derived parameter definition by backfilling from the
/// source readings, then refresh continuous aggregates. Backs the `derived_recompute` trigger.
/// Reads `derived_definition_id` from params.
pub struct DerivedRecompute;

#[async_trait]
impl Job for DerivedRecompute {
    fn name(&self) -> &'static str {
        "derived_recompute"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let derived_id = required_uuid(ctx.params(), "derived_definition_id")?;
        let work = async {
            tracing::info!(derived_id = %derived_id, job_id = %ctx.job_id(), "Recomputing derived parameter");
            let rows = ctx
                .db()
                .query_all(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    r"SELECT DISTINCT r.site_id, r.time
                      FROM readings r
                      JOIN site_parameters sp
                        ON sp.site_id = r.site_id
                       AND sp.is_derived = true
                       AND sp.derived_definition_id = $1
                      JOIN derived_parameter_sources dps
                        ON dps.derived_definition_id = sp.derived_definition_id
                       AND dps.parameter_id = r.parameter_id
                      ORDER BY r.site_id, r.time",
                    [derived_id.into()],
                ))
                .await?;

            let total = i32::try_from(rows.len()).unwrap_or(i32::MAX);
            ctx.set_progress(0, Some(total)).await;

            let mut filled: i32 = 0;
            let mut min_filled: Option<chrono::DateTime<chrono::Utc>> = None;
            for (i, row) in rows.iter().enumerate() {
                if ctx.is_cancelled() {
                    break;
                }
                let Ok(site_id) = row.try_get::<Uuid>("", "site_id") else {
                    continue;
                };
                let Ok(time) = row.try_get::<chrono::DateTime<chrono::FixedOffset>>("", "time")
                else {
                    continue;
                };
                let utc_time = time.with_timezone(&chrono::Utc);
                match recalculate_derived_at_timestamp(ctx.db(), site_id, utc_time).await {
                    Ok(()) => {
                        filled += 1;
                        min_filled = Some(min_filled.map_or(utc_time, |m| m.min(utc_time)));
                    }
                    Err(e) => tracing::error!(error = %e, time = %time, "Failed to recompute derived value"),
                }
                if (i + 1) % 500 == 0 {
                    ctx.set_progress(i as i32 + 1, Some(total)).await;
                }
            }

            if let Some(since) = min_filled {
                tracing::info!(%since, "Refreshing continuous aggregates after derived recompute");
                sync_state::refresh_continuous_aggregates(ctx.db(), Some(since)).await;
            }
            ctx.set_progress(total, Some(total)).await;
            tracing::info!(derived_id = %derived_id, total, filled, "Derived parameter recomputation complete");
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
            .query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT DISTINCT r.time
                  FROM readings r
                  JOIN derived_parameter_sources dps ON dps.parameter_id = r.parameter_id
                  WHERE dps.derived_definition_id = $1 AND r.site_id = $2
                  ORDER BY r.time",
                [def_id.into(), site_id.into()],
            ))
            .await?;

        let mut filled = 0i64;
        let mut earliest: Option<chrono::DateTime<chrono::Utc>> = None;
        for row in &rows {
            if ctx.is_cancelled() {
                break;
            }
            let Ok(time) = row.try_get::<chrono::DateTime<chrono::FixedOffset>>("", "time") else {
                continue;
            };
            let utc = time.with_timezone(&chrono::Utc);
            if recalculate_derived_at_timestamp(ctx.db(), site_id, utc).await.is_ok() {
                filled += 1;
                earliest = Some(earliest.map_or(utc, |e| e.min(utc)));
            }
        }

        if let Some(since) = earliest {
            sync_state::refresh_continuous_aggregates(ctx.db(), Some(since)).await;
        }

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

        let total = i32::try_from(work.iter().map(|(_, ts)| ts.len()).sum::<usize>())
            .unwrap_or(i32::MAX);
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
            sync_state::refresh_continuous_aggregates(ctx.db(), Some(since)).await;
        }
        ctx.set_progress(progress, Some(total)).await;
        tracing::info!(computed = progress, "Derived computation complete");
        Ok(i64::from(progress))
    }
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
        ctx.set_detail(serde_json::json!({
            "scope": { "site_id": site_id },
            "source": { "stream_id": stream_id },
            "counts": { "timestamps": total },
        }))
        .await;
        ctx.set_progress(0, Some(total)).await;

        let mut progress = 0i32;
        for time in timestamps {
            if ctx.is_cancelled() {
                break;
            }
            if let Err(e) = recalculate_derived_at_timestamp(ctx.db(), site_id, time).await {
                tracing::warn!(error = %e, site_id = %site_id, time = %time, "Failed to auto-compute derived values after ingest");
            }
            progress += 1;
            if progress % 500 == 0 {
                ctx.set_progress(progress, Some(total)).await;
            }
        }
        ctx.set_progress(progress, Some(total)).await;
        Ok(i64::from(progress))
    }
}

/// Backdate every `(site, parameter)` slot that has a deployment, re-deriving its readings from the
/// current deployment + calibration timelines. The slot set is recomputed inside the job from
/// `sensor_deployments`, so a rerun always reflects the current deployment topology. Backs the
/// `reprocess_all` operator action. A failed slot logs and continues — a partial backdate is more
/// useful than aborting the whole batch on one bad slot.
pub struct ReprocessAll;

#[async_trait]
impl Job for ReprocessAll {
    fn name(&self) -> &'static str {
        "reprocess_all"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let slot_rows = ctx
            .db()
            .query_all(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT DISTINCT site_id, parameter_id FROM sensor_deployments".to_owned(),
            ))
            .await?;
        let slots: Vec<(Uuid, Uuid)> = slot_rows
            .into_iter()
            .filter_map(|r| {
                let s: Uuid = r.try_get("", "site_id").ok()?;
                let p: Uuid = r.try_get("", "parameter_id").ok()?;
                Some((s, p))
            })
            .collect();
        let slot_count = slots.len();
        ctx.info(&format!("Backdating {slot_count} slot(s)")).await;

        let mut total = 0i64;
        for (site_id, parameter_id) in slots {
            match reprocess_site_parameter_readings(ctx.db(), site_id, parameter_id).await {
                Ok(n) => total += n as i64,
                Err(e) => tracing::warn!(
                    error = %e,
                    site_id = %site_id,
                    parameter_id = %parameter_id,
                    "reprocess_all: slot reprocess failed"
                ),
            }
        }
        ctx.set_detail(serde_json::json!({
            "counts": { "slots": slot_count, "readings_updated": total },
        }))
        .await;
        tracing::info!(readings_updated = total, "reprocess_all complete");
        Ok(total)
    }
}

/// Reconstruct persisted alarm events from the actual readings for the targeted slots and window.
/// All four scoping inputs are optional (`site_id`/`parameter_id`/`start`/`end`); absent inputs
/// widen the scope. Idempotent — re-derives the same episodes. Backs the `rebuild_alarm_events`
/// operator action.
pub struct AlarmBackfill;

#[async_trait]
impl Job for AlarmBackfill {
    fn name(&self) -> &'static str {
        "alarm_backfill"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let params = ctx.params();
        let site_id = optional_uuid(params, "site_id");
        let parameter_id = optional_uuid(params, "parameter_id");
        let start = optional_datetime(params, "start");
        let end = optional_datetime(params, "end");
        let count = crate::routes::private::alarms::episodes::rebuild_alarm_events(
            ctx.db(),
            site_id,
            parameter_id,
            start,
            end,
        )
        .await?;
        if let Some(site_id) = site_id {
            ctx.set_site(site_id).await;
        }
        ctx.set_detail(serde_json::json!({
            "counts": { "events_written": count },
        }))
        .await;
        Ok(count)
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
        let mut total = 0i64;
        for (site_id, parameter_id) in slots {
            match reprocess_site_parameter_readings(ctx.db(), site_id, parameter_id).await {
                Ok(n) => total += n as i64,
                Err(e) => tracing::warn!(
                    error = %e, site_id = %site_id, parameter_id = %parameter_id,
                    "backfill_attribution: slot reprocess failed"
                ),
            }
        }
        ctx.set_detail(serde_json::json!({
            "counts": { "readings_updated": total },
        }))
        .await;
        tracing::info!(readings_updated = total, "backfill_attribution complete");
        Ok(total)
    }
}

/// Re-derive `calibrated_value`/`calibration_id` for the sensors whose identity calibrations the
/// handler just created/backdated. The sensor set is carried in `params.sensors`. Backs the
/// `backfill_calibrations` operator action. A failed sensor logs and continues.
pub struct BackfillCalibrations;

#[async_trait]
impl Job for BackfillCalibrations {
    fn name(&self) -> &'static str {
        "backfill_calibrations"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let sensors = uuid_array(ctx.params(), "sensors");
        let mut total = 0i64;
        for sensor_id in sensors {
            match reprocess_sensor_readings(ctx.db(), sensor_id).await {
                Ok(n) => total += n as i64,
                Err(e) => tracing::warn!(
                    error = %e, sensor_id = %sensor_id,
                    "backfill_calibrations: sensor reprocess failed"
                ),
            }
        }
        ctx.set_detail(serde_json::json!({
            "counts": { "readings_updated": total },
        }))
        .await;
        tracing::info!(readings_updated = total, "backfill_calibrations complete");
        Ok(total)
    }
}

/// Absorb one `site_parameter` into another — moves readings, status events, streams, and
/// deployments, then deletes the source. Idempotent on the readings PK and a no-op DELETE of an
/// absent source, so a rerun is safe. Backs the `merge_site_parameters` operator action.
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
        let result =
            crate::routes::private::admin::merge_services::merge_site_parameters(ctx.db(), &req)
                .await
                .map_err(|e| DbErr::Custom(e.to_string()))?;
        ctx.set_detail(serde_json::json!({ "counts": result })).await;
        Ok(i64::try_from(result.merged_readings).unwrap_or(i64::MAX))
    }
}

/// Absorb one global parameter into another — re-points every `site_parameter`, reading, status
/// event, and stream from source to target, then deletes the source. Idempotent enough to rerun.
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
        let result =
            crate::routes::private::admin::merge_services::merge_parameters(ctx.db(), &req)
                .await
                .map_err(|e| DbErr::Custom(e.to_string()))?;
        ctx.set_detail(serde_json::json!({ "counts": result })).await;
        Ok(i64::try_from(result.readings_moved).unwrap_or(i64::MAX))
    }
}

/// Apply a pairing plan: resolve entities, execute pairings, backfill readings, mark the plan
/// `applied`. The status transition is guarded (only a `draft` plan applies), so a rerun of an
/// already-applied plan is a no-op. Backs the `apply_pairing_plan` operator action.
pub struct PlanApply;

#[async_trait]
impl Job for PlanApply {
    fn name(&self) -> &'static str {
        "plan_apply"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let plan_id = required_uuid(ctx.params(), "plan_id")?;
        let result = crate::routes::private::sync::services::apply_plan(ctx.db(), plan_id)
            .await
            .map_err(|e| DbErr::Custom(e.to_string()))?;
        ctx.set_detail(serde_json::json!({
            "scope": { "plan_id": plan_id },
            "counts": result,
        }))
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
/// mark the plan `reverted`. The status transition is guarded (only an `applied` plan reverts), so
/// a rerun is a no-op. Backs the `revert_pairing_plan` operator action.
pub struct PlanRevert;

#[async_trait]
impl Job for PlanRevert {
    fn name(&self) -> &'static str {
        "plan_revert"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let plan_id = required_uuid(ctx.params(), "plan_id")?;
        let reverted = crate::routes::private::sync::services::revert_plan(ctx.db(), plan_id)
            .await
            .map_err(|e| DbErr::Custom(e.to_string()))?;
        ctx.set_detail(serde_json::json!({
            "scope": { "plan_id": plan_id },
            "counts": { "reverted": reverted },
        }))
        .await;
        ctx.info(&format!("Reverted plan: {reverted} streams unpaired")).await;
        Ok(i64::from(reverted))
    }
}
