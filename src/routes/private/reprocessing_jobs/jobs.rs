//! Concrete `Job` implementations: the worker-run handler for each `trigger_type`. Each reads its
//! inputs from `ctx.params()` and calls the same service function the inline trigger used.

use std::time::Duration;

use async_trait::async_trait;
use sea_orm::{ConnectionTrait, DbErr, EntityTrait, Set, Statement, TransactionTrait};
use uuid::Uuid;

use super::job::Job;
use super::lifecycle::JobContext;
use super::schedule::Schedule;
use crate::common::sync_state;
use crate::config::Config;
use crate::routes::private::readings;
use crate::routes::private::readings::batch::{ConflictMode, readings_on_conflict};
use crate::routes::private::readings::import::BATCH_SIZE as CSV_BATCH_SIZE;
use crate::routes::private::readings::sample_groups;
use crate::routes::private::sensors::calibrations::service::{
    Curve, apply_curves, recalculate_derived_at_timestamp, reprocess_sensor_readings,
    reprocess_site_parameter_readings,
};

/// `Job::run` answers in `DbErr`, the refresh in `AppError`. A refresh that could not run fails
/// the job that asked for it rather than being logged and forgotten.
fn as_db_err(e: crate::error::AppError) -> DbErr {
    DbErr::Custom(e.to_string())
}

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
/// skipped; the persisted params are produced by our own handlers, so this is defensive only.
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
        ctx.info(&format!("Reprocessing readings for sensor {sensor_id}"))
            .await;
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

/// Refresh continuous aggregates, incremental (recent window) or full. Single bounded statement.
pub struct RefreshAggregates {
    name: &'static str,
    full: bool,
}

impl RefreshAggregates {
    #[must_use]
    pub fn incremental() -> Self {
        Self {
            name: "refresh_aggregates",
            full: false,
        }
    }

    #[must_use]
    pub fn full() -> Self {
        Self {
            name: "refresh_aggregates_full",
            full: true,
        }
    }
}

#[async_trait]
impl Job for RefreshAggregates {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        // A refresh that could not run must fail the job: reporting `completed` while the rollups
        // still serve the old numbers is the failure this job exists to make visible.
        let outcome = tokio::time::timeout(Duration::from_secs(600), async {
            if self.full {
                sync_state::refresh_continuous_aggregates_full(ctx.db()).await
            } else {
                sync_state::refresh_continuous_aggregates(ctx.db(), None).await
            }
        })
        .await;
        match outcome {
            Ok(Ok(())) => Ok(0),
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
            reprocess_site_parameter_readings(ctx.db(), site_id, parameter_id).await? as i64;
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
                .query_one(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT parameter_id FROM sensor_deployments \
                     WHERE sensor_id = $1 AND site_id = $2 \
                     ORDER BY (deployed_until IS NULL) DESC, deployed_from DESC LIMIT 1",
                    [sensor_id.into(), site_id.into()],
                ))
                .await?
                .and_then(|r| r.try_get::<Uuid>("", "parameter_id").ok()),
        };
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
                    Err(e) => {
                        tracing::error!(error = %e, time = %time, "Failed to recompute derived value")
                    }
                }
                if (i + 1) % 500 == 0 {
                    ctx.set_progress(i as i32 + 1, Some(total)).await;
                }
            }

            if let Some(since) = min_filled {
                tracing::info!(%since, "Refreshing continuous aggregates after derived recompute");
                sync_state::refresh_continuous_aggregates(ctx.db(), Some(since))
                    .await
                    .map_err(as_db_err)?;
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
            if recalculate_derived_at_timestamp(ctx.db(), site_id, utc)
                .await
                .is_ok()
            {
                filled += 1;
                earliest = Some(earliest.map_or(utc, |e| e.min(utc)));
            }
        }

        if let Some(since) = earliest {
            sync_state::refresh_continuous_aggregates(ctx.db(), Some(since))
                .await
                .map_err(as_db_err)?;
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
        Self {
            interval_seconds: config.janitor_interval_seconds,
            full_refresh_seconds: config.janitor_full_refresh_seconds,
            maintenance_retention_days: config.job_maintenance_retention_days,
            operator_retention_days: config.janitor_retention_days,
            maintenance_max_rows: config.job_maintenance_max_rows,
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
            sync_state::refresh_continuous_aggregates(ctx.db(), Some(since))
                .await
                .map_err(as_db_err)?;
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
/// `reprocess_all` operator action. A failed slot logs and continues, a partial backdate is more
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

/// Reconstruct persisted alarm events from the actual readings. Two scoping shapes:
///
/// - `slots` present (array of `[site_id, parameter_id]`): loop `evaluate_alarm_episodes` over each
///   pair with the shared `start`/`end` window, the per-slot shape the inline batch/CSV ingest
///   spawns used.
/// - `slots` absent: the single/widened `rebuild_alarm_events` path scoped by the optional
///   `site_id`/`parameter_id`/`start`/`end` (the `rebuild_alarm_events` operator action).
///
/// Idempotent either way, re-derives the same episodes.
pub struct AlarmBackfill;

#[async_trait]
impl Job for AlarmBackfill {
    fn name(&self) -> &'static str {
        "alarm_backfill"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let params = ctx.params();
        let start = optional_datetime(params, "start");
        let end = optional_datetime(params, "end");
        let slots = uuid_pair_array(params, "slots");

        if !slots.is_empty() {
            let (Some(start), Some(end)) = (start, end) else {
                return Err(DbErr::Custom(
                    "alarm_backfill with slots requires start and end".into(),
                ));
            };
            let mut total = 0i64;
            for (site_id, parameter_id) in &slots {
                match crate::routes::private::alarms::episodes::evaluate_alarm_episodes(
                    ctx.db(),
                    *site_id,
                    *parameter_id,
                    start,
                    end,
                )
                .await
                {
                    Ok(n) => total += n,
                    Err(e) => tracing::warn!(
                        error = %e, site_id = %site_id, parameter_id = %parameter_id,
                        "alarm backfill slot failed"
                    ),
                }
            }
            if let Some((site_id, _)) = slots.first() {
                ctx.set_site(*site_id).await;
            }
            ctx.set_detail(serde_json::json!({
                "counts": { "events_written": total, "slots": slots.len() },
            }))
            .await;
            return Ok(total);
        }

        let site_id = optional_uuid(params, "site_id");
        let parameter_id = optional_uuid(params, "parameter_id");
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

/// Insert a CSV import's staged readings, recompute derived parameters and refresh aggregates over
/// the imported window, then enqueue an `alarm_backfill` for the touched slots. Reads its inputs
/// (and the staged rows, by `import_token`) from params, so any replica can run it. Non-rerunnable:
/// the staging rows are deleted on completion. Readings constants and the request-level
/// measurement_type are re-applied here, and replicate groups are numbered and given a sample.
pub struct CsvImport;

#[async_trait]
impl Job for CsvImport {
    fn name(&self) -> &'static str {
        "csv_import"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let import_token = required_uuid(ctx.params(), "import_token")?;
        let outcome = Self::run_import(&ctx, import_token).await;
        if outcome.is_err() {
            // Success deletes the staging rows below; a mid-run error would otherwise orphan them
            // (there is no janitor for csv_import_staging), so drop them on the failure path too.
            let _ = ctx
                .db()
                .execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "DELETE FROM csv_import_staging WHERE import_token = $1",
                    [import_token.into()],
                ))
                .await;
        }
        outcome
    }
}

impl CsvImport {
    async fn run_import(ctx: &JobContext, import_token: Uuid) -> Result<i64, DbErr> {
        let params = ctx.params();
        let site_id = required_uuid(params, "site_id")?;
        let site_name = params
            .get("site_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let conflict = match params.get("conflict").and_then(serde_json::Value::as_str) {
            Some("overwrite") => ConflictMode::Overwrite,
            _ => ConflictMode::Skip,
        };
        let since = optional_datetime(params, "since");
        let latest = optional_datetime(params, "latest");
        let overlapping = params
            .get("overlapping")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize;
        let overlap_differing = params
            .get("overlap_differing")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize;
        let param_streams = uuid_pair_array(params, "param_streams");
        // Explicit request-level classification, or None to resolve per row from the
        // stream declaration and the owning sensor's data_frequency.
        let request_measurement_type = params
            .get("measurement_type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        ctx.set_site(site_id).await;

        // Read the staged rows back and rebuild the readings, re-applying the constant fields.
        let staged = ctx
            .db()
            .query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT stream_id, site_id, parameter_id, time, raw_value, \
                        sensor_id, calibration_id, deployment_id \
                 FROM csv_import_staging WHERE import_token = $1 ORDER BY seq",
                [import_token.into()],
            ))
            .await?;

        // Staging carries the calibration each row resolved at upload time. The coefficients are
        // read back here so the stored value is the one that calibration produces: a row that names
        // a curve and carries the uncorrected number claims a correction it never had.
        let staged_curves = {
            let mut ids: Vec<Uuid> = staged
                .iter()
                .filter_map(|row| {
                    row.try_get::<Option<Uuid>>("", "calibration_id")
                        .ok()
                        .flatten()
                })
                .collect();
            ids.sort_unstable();
            ids.dedup();
            let mut curves: std::collections::HashMap<Uuid, Curve> =
                std::collections::HashMap::new();
            if !ids.is_empty() {
                for row in ctx
                    .db()
                    .query_all(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "SELECT id, slope, intercept FROM sensor_calibrations WHERE id = ANY($1)",
                        [ids.into()],
                    ))
                    .await?
                {
                    let id: Uuid = row.try_get("", "id")?;
                    curves.insert(
                        id,
                        Curve {
                            id,
                            slope: row.try_get("", "slope")?,
                            intercept: row.try_get("", "intercept")?,
                        },
                    );
                }
            }
            curves
        };

        let (stream_defaults, sensor_types) = if request_measurement_type.is_none() {
            let mut stream_ids: Vec<Uuid> = Vec::new();
            let mut sensor_ids: Vec<Uuid> = Vec::new();
            for row in &staged {
                stream_ids.push(row.try_get("", "stream_id")?);
                if let Some(sid) = row.try_get::<Option<Uuid>>("", "sensor_id")? {
                    sensor_ids.push(sid);
                }
            }
            stream_ids.sort_unstable();
            stream_ids.dedup();
            sensor_ids.sort_unstable();
            sensor_ids.dedup();

            let mut defaults: std::collections::HashMap<Uuid, Option<String>> =
                std::collections::HashMap::new();
            for row in ctx
                .db()
                .query_all(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT id, measurement_type FROM data_streams WHERE id = ANY($1)",
                    [stream_ids.into()],
                ))
                .await?
            {
                let id: Uuid = row.try_get("", "id")?;
                defaults.insert(id, row.try_get("", "measurement_type")?);
            }
            let types =
                crate::routes::private::readings::measurement::measurement_types_for_sensors(
                    ctx.db(),
                    &sensor_ids,
                )
                .await?;
            (defaults, types)
        } else {
            (
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            )
        };

        let mut models: Vec<readings::ActiveModel> = Vec::with_capacity(staged.len());
        let mut distinct_ts: Vec<chrono::DateTime<chrono::Utc>> = Vec::new();
        for row in &staged {
            let stream_id: Uuid = row.try_get("", "stream_id")?;
            let row_site_id: Option<Uuid> = row.try_get("", "site_id")?;
            let parameter_id: Option<Uuid> = row.try_get("", "parameter_id")?;
            let time: chrono::DateTime<chrono::FixedOffset> = row.try_get("", "time")?;
            let raw_value: f64 = row.try_get("", "raw_value")?;
            let sensor_id: Option<Uuid> = row.try_get("", "sensor_id")?;
            let calibration_id: Option<Uuid> = row.try_get("", "calibration_id")?;
            let deployment_id: Option<Uuid> = row.try_get("", "deployment_id")?;
            distinct_ts.push(time.with_timezone(&chrono::Utc));
            let measurement_type =
                crate::routes::private::readings::measurement::resolve_measurement_type(
                    request_measurement_type.as_deref(),
                    stream_defaults.get(&stream_id).and_then(|d| d.as_deref()),
                    sensor_id,
                    &sensor_types,
                );
            // A value is only ever what a curve produced: with none resolved the column stays
            // NULL, the same as an entry through `/grab_samples` or `/ingest`, so an uncorrected
            // measurement is never mistaken for a corrected one. Consumers read
            // COALESCE(calibrated_value, raw_value), so the raw value is still what is served.
            let base = calibration_id
                .and_then(|id| staged_curves.get(&id))
                .copied();
            let calibrated_value = base.map(|c| apply_curves(raw_value, Some(c), None));
            models.push(readings::ActiveModel {
                standard_curve_id: Set(None),
                stream_id: Set(stream_id),
                site_id: Set(row_site_id),
                parameter_id: Set(parameter_id),
                time: Set(time),
                replicate_index: Set(0),
                raw_value: Set(raw_value),
                calibrated_value: Set(calibrated_value),
                sensor_id: Set(sensor_id),
                calibration_id: Set(calibration_id),
                deployment_id: Set(deployment_id),
                logged: Set(Some(false)),
                measurement_type: Set(Some(measurement_type)),
                is_flagged: Set(Some(false)),
                flag_reason: Set(None),
                sample_id: Set(None),
            });
        }
        distinct_ts.sort_unstable();
        distinct_ts.dedup();

        // Rows sharing (stream_id, time) are numbered replicate_index 0..n-1 in staging seq order.
        let mut group_counts: std::collections::HashMap<
            (Uuid, chrono::DateTime<chrono::Utc>),
            i16,
        > = std::collections::HashMap::with_capacity(models.len());
        for m in &mut models {
            let key = (
                *m.stream_id.as_ref(),
                m.time.as_ref().with_timezone(&chrono::Utc),
            );
            let counter = group_counts.entry(key).or_insert(0);
            m.replicate_index = Set(*counter);
            *counter += 1;
        }

        // Whether a group is a sample is not decided here: `sample_groups::forms_sample` is the
        // one answer. A request-level `spot` is the writer declaring a collection event, so a
        // single row is a grab that has to reach the views reading `samples`; without that
        // declaration only two or more rows classified spot on a paired slot form one, because two
        // logger points sharing a timestamp are a malformed file rather than a sampling event.
        let declared_collection = request_measurement_type.as_deref() == Some(sample_groups::SPOT);
        let mut spot_groups: std::collections::HashMap<
            (Uuid, Uuid, chrono::DateTime<chrono::Utc>),
            usize,
        > = std::collections::HashMap::new();
        for m in &models {
            if m.measurement_type.as_ref().as_deref() != Some(sample_groups::SPOT) {
                continue;
            }
            if let (Some(sid), Some(pid)) = (*m.site_id.as_ref(), *m.parameter_id.as_ref()) {
                *spot_groups
                    .entry((sid, pid, m.time.as_ref().with_timezone(&chrono::Utc)))
                    .or_default() += 1;
            }
        }
        let replicate_groups = spot_groups
            .values()
            .filter(|count| sample_groups::forms_sample(declared_collection, **count))
            .count();

        let total = i32::try_from(models.len()).unwrap_or(i32::MAX);
        ctx.set_progress(0, Some(total)).await;

        // Phase 1: insert readings.
        let mut affected_total = 0usize;
        let mut inserted_so_far = 0usize;
        for chunk in models.chunks(CSV_BATCH_SIZE) {
            match readings::Entity::insert_many(chunk.to_vec())
                .on_conflict(readings_on_conflict(conflict))
                .exec_without_returning(ctx.db())
                .await
            {
                Ok(affected) => affected_total += affected as usize,
                Err(e) => {
                    let msg = e.to_string();
                    if !msg.contains("None of the records") {
                        tracing::warn!(error = %e, "Failed to insert imported readings chunk");
                        return Err(e);
                    }
                }
            }
            inserted_so_far += chunk.len();
            if inserted_so_far % 5000 < CSV_BATCH_SIZE {
                ctx.set_progress(
                    i32::try_from(inserted_so_far).unwrap_or(i32::MAX),
                    Some(total),
                )
                .await;
            }
        }

        // An overwrite replaces the measurement, not the correction: an import never decides which
        // curve applies, so the corrected raw value goes back through the curves already on the row.
        if conflict == ConflictMode::Overwrite
            && overlapping > 0
            && let (Some(first), Some(last)) = (distinct_ts.first(), distinct_ts.last())
        {
            let mut stream_ids: Vec<Uuid> = models.iter().map(|m| *m.stream_id.as_ref()).collect();
            stream_ids.sort_unstable();
            stream_ids.dedup();
            let recomposed =
                crate::routes::private::sensors::calibrations::service::recompose_from_own_curves(
                    ctx.db(),
                    "TRUE",
                    "r.stream_id = ANY($1) AND r.time >= $2 AND r.time <= $3",
                    vec![
                        stream_ids.into(),
                        sea_orm::prelude::DateTimeWithTimeZone::from(*first).into(),
                        sea_orm::prelude::DateTimeWithTimeZone::from(*last).into(),
                    ],
                )
                .await?;
            tracing::info!(
                site = %site_name,
                recomposed,
                "CSV overwrite recomposed corrected values from the curves already on the rows"
            );
        }

        // Samples are found-or-created and stamped after the insert, by the one materialiser, over
        // the streams and the time span this import touched. Scoping it that way rather than to the
        // rows this run inserted is deliberate: a reading already present at a group's slot is part
        // of the same collection event, whose identity is (site, parameter, instant).
        if !spot_groups.is_empty()
            && let (Some(first), Some(last)) = (distinct_ts.first(), distinct_ts.last())
        {
            let mut stream_ids: Vec<Uuid> = models.iter().map(|m| *m.stream_id.as_ref()).collect();
            stream_ids.sort_unstable();
            stream_ids.dedup();
            let row_predicate = format!(
                "r.stream_id = ANY($1) AND r.time >= '{}'::timestamptz AND r.time <= '{}'::timestamptz",
                first.to_rfc3339(),
                last.to_rfc3339()
            );
            sample_groups::materialise_samples(
                ctx.db(),
                &row_predicate,
                vec![stream_ids.into()],
                declared_collection,
            )
            .await
            .map_err(as_db_err)?;
        }

        let (inserted_total, overwritten) = match conflict {
            ConflictMode::Skip => (affected_total, 0),
            ConflictMode::Overwrite => (
                affected_total.saturating_sub(overlapping),
                overlap_differing,
            ),
        };
        tracing::info!(site = %site_name, inserted_total, overwritten, "CSV import inserted readings");

        if inserted_total > 0 || overwritten > 0 {
            for (parameter_id, stream_id) in &param_streams {
                let _ = ctx.events().send(crate::common::AppEvent::DataIngested {
                    site_id: Some(site_id),
                    parameter_id: Some(*parameter_id),
                    stream_id: *stream_id,
                    count: inserted_total + overwritten,
                });
            }

            // Phase 2: derived recompute over the imported timestamps.
            let derived_total = i32::try_from(models.len() + distinct_ts.len()).unwrap_or(i32::MAX);
            ctx.set_progress(
                i32::try_from(models.len()).unwrap_or(i32::MAX),
                Some(derived_total),
            )
            .await;
            for (i, time) in distinct_ts.iter().enumerate() {
                if ctx.is_cancelled() {
                    break;
                }
                let _ = recalculate_derived_at_timestamp(ctx.db(), site_id, *time).await;
                if (i + 1) % 500 == 0 {
                    let prog = i32::try_from(models.len() + i + 1).unwrap_or(i32::MAX);
                    ctx.set_progress(prog, Some(derived_total)).await;
                }
            }

            if let Some(s) = since {
                sync_state::refresh_continuous_aggregates(ctx.db(), Some(s))
                    .await
                    .map_err(as_db_err)?;
            }
            if let Some(app) = crate::common::global_app_state() {
                crate::common::cache::invalidate_prefix(&app, &format!("readings:{site_id}")).await;
                crate::common::cache::invalidate_prefix(&app, &format!("aggregates:{site_id}"))
                    .await;
            }

            // Rebuild persisted alarm events for the imported window so out-of-range CSV rows become
            // breach episodes. Enqueued as a separate `alarm_backfill` job (not spawned inline) so it
            // runs on the worker pool too.
            if let (Some(alarm_start), Some(alarm_end)) = (since, latest) {
                let slots: Vec<serde_json::Value> = param_streams
                    .iter()
                    .map(|(pid, _)| serde_json::json!([site_id, pid]))
                    .collect();
                crate::routes::private::reprocessing_jobs::worker::enqueue(
                    ctx.db(),
                    "alarm_backfill",
                    None,
                    None,
                    &serde_json::json!({
                        "slots": slots,
                        "start": alarm_start.to_rfc3339(),
                        "end": alarm_end.to_rfc3339(),
                    }),
                    None,
                )
                .await?;
            }
        }

        // The staging source has served its purpose, drop it (makes this job non-rerunnable).
        ctx.db()
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "DELETE FROM csv_import_staging WHERE import_token = $1",
                [import_token.into()],
            ))
            .await?;

        ctx.set_detail(serde_json::json!({
            "scope": { "site_id": site_id },
            "counts": {
                "inserted": inserted_total,
                "overwritten": overwritten,
                "replicate_groups": replicate_groups,
            },
        }))
        .await;
        Ok(i64::from(
            i32::try_from(inserted_total + overwritten).unwrap_or(i32::MAX),
        ))
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

/// Absorb one `site_parameter` into another, moves readings, status events, streams, and
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
        ctx.set_detail(serde_json::json!({ "counts": result }))
            .await;
        Ok(i64::try_from(result.merged_readings).unwrap_or(i64::MAX))
    }
}

/// Absorb one global parameter into another, re-points every `site_parameter`, reading, status
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
        ctx.set_detail(serde_json::json!({ "counts": result }))
            .await;
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
        let result = crate::routes::private::sync::service::apply_plan(ctx.db(), plan_id)
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
        let reverted = crate::routes::private::sync::service::revert_plan(ctx.db(), plan_id)
            .await
            .map_err(|e| DbErr::Custom(e.to_string()))?;
        ctx.set_detail(serde_json::json!({
            "scope": { "plan_id": plan_id },
            "counts": { "reverted": reverted },
        }))
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
    fn validate(&self, tunables: &serde_json::Value) -> Result<(), String> {
        if tunables.is_null() {
            return Ok(());
        }
        let Some(obj) = tunables.as_object() else {
            return Err("tunables must be a JSON object".to_string());
        };
        if let Some(v) = obj.get("retention_days") {
            let ok = v.as_u64().is_some_and(|n| n >= 1);
            if !ok {
                return Err("retention_days must be a positive integer".to_string());
            }
        }
        Ok(())
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        use crate::routes::private::parameters::derived::janitor;
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

        // 1. Fill derived gaps, reporting into this job and refreshing aggregates back to the
        //    earliest filled timestamp.
        if let Err(e) = janitor::run_once(db, Some(&ctx)).await {
            tracing::warn!(error = %e, "Derived janitor: run failed");
        }

        // 2. Repair corrected readings whose stored value is no longer what their own curves
        //    produce, whichever route moved them apart. Hooks make that repair immediate; this makes
        //    it eventual, so a hook that never fired costs staleness rather than a wrong number.
        //    Refreshed over the span it moved, before the rollups below settle for this tick.
        let mut recomposed = 0u64;
        match crate::routes::private::sensors::calibrations::service::sweep_curve_drift(db).await {
            Ok(drift) if drift.moved > 0 => {
                recomposed = drift.moved;
                tracing::info!(moved = drift.moved, "Janitor: recomposed drifted curve values");
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
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "Janitor: curve drift sweep failed"),
        }

        // 3. A full continuous-aggregate refresh opens each `full_refresh_seconds` period and an
        //    incremental one runs otherwise. The tick that carries it is the one whose scheduled
        //    slot falls in the first cadence window of the period, and the cadence is the
        //    `schedules` row's, not the process's: the scheduler stamps both the slot and the
        //    interval it fired on into the job params, so an operator cadence change cannot leave
        //    the full refresh unreachable. A `run_now` carries neither and falls back to the
        //    wall clock and the configured interval. A cadence longer than `full_refresh_seconds`
        //    makes every tick a full refresh, which is the safe direction but is a real cost on a
        //    large database.
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
        if do_full {
            tracing::info!("Derived janitor: running scheduled full continuous aggregate refresh");
            sync_state::refresh_continuous_aggregates_full(db)
                .await
                .map_err(as_db_err)?;
        } else {
            sync_state::refresh_continuous_aggregates(db, None)
                .await
                .map_err(as_db_err)?;
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
        ctx.set_detail(serde_json::json!({
            "scope": { "full_refresh": do_full },
            "counts": { "recomposed": recomposed, "pruned": pruned },
        }))
        .await;
        Ok(pruned as i64)
    }
}

/// Reconcile persisted `alarm_events` against the current breach set (open/update/resolve), then
/// emit an `AlarmStateChanged` SSE on change, the alarm-sweeper backstop. Wraps
/// [`sweeper::evaluate_alarm_events`] + the same SSE the old `sweeper::periodic` emitted.
pub struct AlarmSweep {
    interval_seconds: u64,
}

impl AlarmSweep {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            interval_seconds: config.alarm_sweep_interval_seconds,
        }
    }
}

#[async_trait]
impl Job for AlarmSweep {
    fn name(&self) -> &'static str {
        "alarm_sweep"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(self.interval_seconds.max(1) as i64))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        use crate::routes::private::alarms::sweeper;
        match sweeper::evaluate_alarm_events(ctx.db()).await {
            Ok(stats) => {
                if (stats.opened > 0 || stats.resolved > 0)
                    && let Some(events) = crate::common::global_event_sender()
                {
                    let _ = events.send(crate::common::AppEvent::AlarmStateChanged {
                        opened: stats.opened,
                        resolved: stats.resolved,
                    });
                }
                Ok((stats.opened + stats.resolved) as i64)
            }
            Err(e) => Err(DbErr::Custom(format!("alarm sweep failed: {e}"))),
        }
    }
}

/// Close sync_events rows left 'running' past a staleness threshold. A sync service killed
/// mid-cycle (SIGKILL, node loss) can never terminate its own event; without this sweep the
/// row reads as "sync in progress" forever.
pub struct SyncEventSweep {
    interval_seconds: u64,
    stale_after_seconds: u64,
}

impl SyncEventSweep {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            interval_seconds: config.sync_event_sweep_interval_seconds,
            stale_after_seconds: config.sync_event_stale_after_seconds,
        }
    }
}

#[async_trait]
impl Job for SyncEventSweep {
    fn name(&self) -> &'static str {
        "sync_event_sweep"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(self.interval_seconds.max(1) as i64))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let closed = sweep_stale_sync_events(ctx.db(), self.stale_after_seconds).await?;
        Ok(closed as i64)
    }
}

/// Close 'running' sync_events older than the staleness threshold; returns the row count.
pub async fn sweep_stale_sync_events(
    db: &sea_orm::DatabaseConnection,
    stale_after_seconds: u64,
) -> Result<u64, DbErr> {
    let res = db
        .execute(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE sync_events
             SET status = 'failed',
                 completed_at = NOW(),
                 errors = COALESCE(errors, '[]'::jsonb) || '[\"Closed by sweeper: service stopped reporting\"]'::jsonb
             WHERE status = 'running' AND started_at < NOW() - ($1 || ' seconds')::interval",
            [stale_after_seconds.to_string().into()],
        ))
        .await?;
    Ok(res.rows_affected())
}

/// Re-resolve every active linked Telegram identity against Keycloak and deactivate any whose user
/// is gone/disabled/role-revoked, the anti-backdoor identity reconciliation. Wraps
/// [`reconcile::sweep`]. Needs the live `AppState` (Keycloak admin proxy + the shared `Authorizer`
/// cache), read from the process global; a no-op when no `AppState` was built (some test contexts).
pub struct IdentityReconcile {
    interval_seconds: u64,
}

impl IdentityReconcile {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            interval_seconds: config.identity_reconcile_interval_seconds,
        }
    }
}

#[async_trait]
impl Job for IdentityReconcile {
    fn name(&self) -> &'static str {
        "identity_reconcile"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(self.interval_seconds.max(1) as i64))
    }

    async fn run(&self, _ctx: JobContext) -> Result<i64, DbErr> {
        let Some(state) = crate::common::global_app_state() else {
            tracing::debug!("identity_reconcile: no AppState in process; skipping");
            return Ok(0);
        };
        match crate::routes::private::notifications::reconcile::sweep(&state).await {
            Ok(0) => Ok(0),
            Ok(n) => {
                tracing::warn!(
                    count = n,
                    "Identity reconciliation: deactivated revoked links"
                );
                Ok(n as i64)
            }
            Err(e) => Err(e),
        }
    }
}

/// Probe each configured notification channel (Telegram `getMe` / SMTP / Graph token) and upsert
/// `notification_channel_health`, the channel health heartbeat. Wraps [`health::probe_once`].
pub struct NotifyHealth {
    interval_seconds: u64,
}

impl NotifyHealth {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            interval_seconds: config.notify_health_interval_seconds.max(30),
        }
    }
}

#[async_trait]
impl Job for NotifyHealth {
    fn name(&self) -> &'static str {
        "notify_health"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(self.interval_seconds.max(1) as i64))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let Some(state) = crate::common::global_app_state() else {
            tracing::debug!("notify_health: no AppState in process; skipping");
            return Ok(0);
        };
        crate::routes::private::notifications::health::probe_once(ctx.db(), &state.config).await;
        Ok(0)
    }
}

/// Drain the `alarm_events` notification outbox (open + resolve passes) and run the signal triggers,
/// the notification dispatcher. Wraps [`dispatcher::dispatch_once`]. The `AlarmStateChanged`
/// broadcast still wakes an immediate enqueue in `main.rs` for low latency; this schedule is the
/// fallback cadence.
pub struct DispatchNotifications {
    interval_seconds: u64,
}

impl DispatchNotifications {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            interval_seconds: config.notify_poll_interval_seconds,
        }
    }
}

#[async_trait]
impl Job for DispatchNotifications {
    fn name(&self) -> &'static str {
        "dispatch_notifications"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(self.interval_seconds.max(1) as i64))
    }

    async fn run(&self, _ctx: JobContext) -> Result<i64, DbErr> {
        use crate::routes::private::notifications::dispatcher;
        let Some(state) = crate::common::global_app_state() else {
            tracing::debug!("dispatch_notifications: no AppState in process; skipping");
            return Ok(0);
        };
        let channels = dispatcher::build_channels(&state.config);
        dispatcher::dispatch_once(&state, &channels).await;
        Ok(0)
    }
}

/// Retag readings.measurement_type for a sensor/stream scope, then refresh continuous aggregates
/// over the affected window. Backs the bulk reclassification actions (mark sensors low/high
/// frequency, classify sensorless streams): the classification columns (`sensors.data_frequency`,
/// `data_streams.measurement_type`) are updated synchronously by the endpoint; this job makes the
/// existing rows agree. Rerunnable (idempotent, the UPDATE skips rows already at the target).
/// Decompression-safe: portal/lab history lives in compressed (>30-day) chunks.
pub struct MeasurementRetag;

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
            .filter(|t| matches!(*t, "continuous" | "spot" | "derived" | "declared"))
            .ok_or_else(|| DbErr::Custom("measurement_retag needs target".to_string()))?
            .to_string();
        // 'declared' aligns each reading with its own stream's classification, for source systems
        // that mix grab and logger columns.
        let declared = target == "declared";
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

        // 'declared' joins each reading to its stream in both the window probe and the rewrite and
        // drops the target parameter; a fixed target compares against $1.
        // The sensor arm also matches by stream ownership: readings ingested before attribution
        // backfill carry sensor_id NULL but belong to the sensor's streams all the same.
        let (scope, from_clause, mismatch, new_value, update_from) = if declared {
            (
                "(r.sensor_id = ANY($1) OR r.stream_id = ANY($2) \
                  OR r.stream_id IN (SELECT id FROM data_streams WHERE sensor_id = ANY($1)) \
                  OR ($3::text IS NOT NULL AND r.stream_id IN \
                      (SELECT id FROM data_streams WHERE source_system = $3)))",
                ", data_streams ds",
                "r.stream_id = ds.id AND ds.measurement_type IS NOT NULL \
                 AND r.measurement_type IS DISTINCT FROM ds.measurement_type",
                "ds.measurement_type",
                "FROM data_streams ds",
            )
        } else {
            (
                "(r.sensor_id = ANY($2) OR r.stream_id = ANY($3) \
                  OR r.stream_id IN (SELECT id FROM data_streams WHERE sensor_id = ANY($2)) \
                  OR ($4::text IS NOT NULL AND r.stream_id IN \
                      (SELECT id FROM data_streams WHERE source_system = $4)))",
                "",
                "r.measurement_type IS DISTINCT FROM $1",
                "$1",
                "",
            )
        };
        let mut values: Vec<sea_orm::Value> = Vec::new();
        if !declared {
            values.push(target.clone().into());
        }
        values.push(sensor_ids.clone().into());
        values.push(stream_ids.clone().into());
        values.push(source_system.clone().into());

        // Affected window (for the aggregate refresh), read before the rewrite.
        let window = ctx
            .db()
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                &format!(
                    "SELECT min(r.time) AS lo, max(r.time) AS hi FROM readings r{from_clause} \
                     WHERE {mismatch} AND {scope}"
                ),
                values.clone(),
            ))
            .await?;
        let (lo, hi) = match &window {
            Some(row) => (
                row.try_get::<Option<chrono::DateTime<chrono::FixedOffset>>>("", "lo")?,
                row.try_get::<Option<chrono::DateTime<chrono::FixedOffset>>>("", "hi")?,
            ),
            None => (None, None),
        };
        let (Some(lo), Some(hi)) = (lo, hi) else {
            ctx.info("Nothing to retag, every reading in scope already matches")
                .await;
            return Ok(0);
        };

        // A stream declaring a different classification will keep writing its own value on
        // ingest, so the retag would drift back; surface the conflict in the job timeline.
        if !declared {
            let conflicting = ctx
                .db()
                .query_all(Statement::from_sql_and_values(
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
                let system: String = row.try_get("", "source_system")?;
                let key: String = row.try_get("", "source_key")?;
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
        let txn = ctx.db().begin().await?;
        txn.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0".to_owned(),
        ))
        .await?;
        let retagged = txn
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                &format!(
                    "UPDATE readings r SET measurement_type = {new_value} \
                     {update_from} WHERE {mismatch} AND {scope}"
                ),
                values,
            ))
            .await?
            .rows_affected();
        txn.commit().await?;

        // Membership in the rollups changed (spot/derived are excluded), so refresh every
        // aggregate over the affected window. Widened by one bucket so monthly boundaries are
        // fully covered; CALL runs outside the txn (procedures can't run inside one).
        let refresh_lo = lo - chrono::Duration::days(32);
        let refresh_hi = hi + chrono::Duration::days(32);
        for agg in [
            "readings_hourly",
            "readings_daily",
            "readings_weekly",
            "readings_monthly",
        ] {
            if let Err(e) = ctx
                .db()
                .execute(Statement::from_string(
                    sea_orm::DatabaseBackend::Postgres,
                    format!(
                        "CALL refresh_continuous_aggregate('{agg}', '{}'::timestamptz, '{}'::timestamptz)",
                        refresh_lo.to_rfc3339(),
                        refresh_hi.to_rfc3339()
                    ),
                ))
                .await
            {
                ctx.log(
                    "warn",
                    &format!("Failed to refresh {agg} after retag"),
                    serde_json::json!({ "error": e.to_string() }),
                )
                .await;
            }
        }

        // Reclassified rows change what bounded cached responses would serve.
        if retagged > 0
            && let Some(state) = crate::common::global_app_state()
        {
            state.response_cache.invalidate_all();
        }

        ctx.set_detail(serde_json::json!({
            "counts": { "readings_retagged": retagged },
            "target": target,
            "window": { "from": lo.to_rfc3339(), "until": hi.to_rfc3339() },
        }))
        .await;
        Ok(retagged.try_into().unwrap_or(i64::MAX))
    }
}
