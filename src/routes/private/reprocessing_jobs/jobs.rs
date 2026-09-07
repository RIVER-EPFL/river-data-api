//! Concrete `Job` implementations: the worker-run handler for each `trigger_type`. Each reads its
//! inputs from `ctx.params()` and calls the same service function the inline trigger used.

use std::time::Duration;

use async_trait::async_trait;
use sea_orm::{ConnectionTrait, DbErr, EntityTrait, Set, Statement};
use uuid::Uuid;

use super::job::{Job, TunableKind, TunableSpec};
use super::lifecycle::{JobContext, JobReport};
use super::schedule::Schedule;
use crate::common::sync_state;
use crate::config::Config;
use crate::routes::private::readings;
use crate::routes::private::readings::batch::{ConflictMode, readings_on_conflict};
use crate::routes::private::readings::import::BATCH_SIZE as CSV_BATCH_SIZE;
use crate::routes::private::readings::sample_groups;
use crate::routes::private::readings::tail;
use crate::routes::private::sensors::calibrations::service::{
    Curve, apply_curves, recalculate_derived_at_timestamp, reprocess_sensor_readings,
    reprocess_site_parameter_readings,
};

/// `Job::run` answers in `DbErr`, the refresh in `AppError`. A refresh that could not run fails
/// the job that asked for it rather than being logged and forgotten.
fn as_db_err(e: crate::error::AppError) -> DbErr {
    DbErr::Custom(e.to_string())
}

/// Take a spot group's replicates from `count` onwards out of what the group serves, ahead of an
/// overwrite that carries only `count` columns.
///
/// The displacement is a `withdrawn_at` stamp, not a delete: a flag, a hand-picked standard curve
/// and the value itself all survive it, and an import that later carries the column again clears
/// the stamp. A displaced row somebody had curated raises a `source_modified` hold, because
/// dropping it out of the served group is a ruling the file's column count should not make alone.
async fn displace_spot_tail(
    txn: &sea_orm::DatabaseTransaction,
    stream_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
    count: i16,
) -> crate::error::AppResult<()> {
    let time_value = sea_orm::prelude::DateTimeWithTimeZone::from(time);
    let curated = txn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT replicate_index, \
                    CASE WHEN is_flagged IS TRUE THEN 'flagged' ELSE 'standard_curve' END AS reason \
             FROM readings \
             WHERE stream_id = $1 AND time = $2 AND replicate_index >= $3 \
               AND measurement_type = 'spot' AND withdrawn_at IS NULL \
               AND (is_flagged IS TRUE OR standard_curve_id IS NOT NULL) \
             ORDER BY replicate_index",
            [
                stream_id.into(),
                time_value.into(),
                count.into(),
            ],
        ))
        .await?;
    if !curated.is_empty() {
        let entries = curated
            .iter()
            .map(|row| {
                Ok(serde_json::json!({
                    "replicate_index": row.try_get::<i16>("", "replicate_index")?,
                    "reason": row.try_get::<String>("", "reason")?,
                }))
            })
            .collect::<Result<Vec<_>, DbErr>>()?;
        crate::routes::private::readings::reconcile::upsert_source_modified_hold(
            txn,
            stream_id,
            time,
            serde_json::json!({ "claim": "displaced", "withdrawn": entries }),
            serde_json::json!({ "replicates": count }),
        )
        .await?;
    }

    // The displacement and its reversal are decisions of CSV origin (ADR 0008).
    use crate::routes::private::readings::decisions::{Kind, NewValue, Origin, record_many};
    record_many(
        txn,
        Kind::Withdraw,
        "r.stream_id = $1 AND r.time = $2 AND r.replicate_index >= $3 \
         AND r.measurement_type = 'spot' AND r.withdrawn_at IS NULL",
        vec![stream_id.into(), time_value.clone().into(), count.into()],
        NewValue::Literal(serde_json::json!({ "reason": "displaced_by_overwrite" })),
        "csv_import",
        Some("displaced_by_overwrite"),
        Origin::Csv,
        None,
    )
    .await
    .map_err(as_db_err)?;

    // A file that carries the column again re-asserts it, so its own displacement is reversed.
    record_many(
        txn,
        Kind::Reassert,
        "r.stream_id = $1 AND r.time = $2 AND r.replicate_index < $3 \
         AND r.withdrawn_reason = 'displaced_by_overwrite'",
        vec![stream_id.into(), time_value.into(), count.into()],
        NewValue::Literal(serde_json::json!({})),
        "csv_import",
        Some("re-asserted by the file"),
        Origin::Csv,
        None,
    )
    .await
    .map_err(as_db_err)?;
    Ok(())
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
    async fn record(&self, ctx: &JobContext, report: JobReport) -> JobReport {
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

    fn error(&self) -> DbErr {
        DbErr::Custom(format!("every one of {} slots failed", self.failed.len()))
    }
}

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
            .query_one_raw(Statement::from_sql_and_values(
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
        ctx.report(
            JobReport::new()
                .scope("sensor_id", sensor_id.to_string())
                .count("readings_updated", count),
        )
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
            Ok(Ok(())) => {
                ctx.report(JobReport::new().scope("full_refresh", self.full))
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
            reprocess_site_parameter_readings(ctx.db(), site_id, parameter_id).await? as i64;
        if let Some(sensor_id) = optional_uuid(ctx.params(), "sensor_id") {
            reprocess_sensor_readings(ctx.db(), sensor_id).await?;
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
                .and_then(|r| r.try_get::<Uuid>("", "parameter_id").ok()),
        };
        let count = if let Some(parameter_id) = parameter_id {
            reprocess_site_parameter_readings(ctx.db(), site_id, parameter_id).await? as i64
        } else {
            0
        };
        reprocess_sensor_readings(ctx.db(), sensor_id).await?;
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
                .query_all_raw(Statement::from_sql_and_values(
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
            let mut filled_sites: std::collections::BTreeSet<Uuid> =
                std::collections::BTreeSet::new();
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
                        filled_sites.insert(site_id);
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
                for site_id in filled_sites {
                    announce_derived_write(&ctx, site_id, filled);
                }
            }
            ctx.set_progress(total, Some(total)).await;
            ctx.report(
                JobReport::new()
                    .scope("derived_definition_id", derived_id.to_string())
                    .scope_opt("earliest_filled", min_filled.map(|t| t.to_rfc3339()))
                    .count("timestamps", total)
                    .count("filled", filled),
            )
            .await;
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
            .query_all_raw(Statement::from_sql_and_values(
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
            sync_state::refresh_continuous_aggregates(ctx.db(), Some(since))
                .await
                .map_err(as_db_err)?;
            announce_derived_write(&ctx, site_id, progress);
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
            .query_all_raw(Statement::from_string(
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

        let mut results = Vec::with_capacity(slot_count);
        for (site_id, parameter_id) in slots {
            let moved = reprocess_site_parameter_readings(ctx.db(), site_id, parameter_id)
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
            let mut results = Vec::with_capacity(slots.len());
            for (site_id, parameter_id) in &slots {
                let written = crate::routes::private::alarms::episodes::evaluate_alarm_episodes(
                    ctx.db(),
                    *site_id,
                    *parameter_id,
                    start,
                    end,
                )
                .await;
                results.push((
                    serde_json::json!({ "site_id": site_id, "parameter_id": parameter_id }),
                    written,
                ));
            }
            if let Some((site_id, _)) = slots.first() {
                ctx.set_site(*site_id).await;
            }
            let outcome = SlotOutcome::from(results);
            let total = outcome.readings;
            let report = outcome
                .record(
                    &ctx,
                    JobReport::new()
                        .count("events_written", total)
                        .count("slots", slots.len()),
                )
                .await;
            ctx.report(report).await;
            if outcome.all_failed() {
                return Err(outcome.error());
            }
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
        ctx.report(
            JobReport::new()
                .scope_opt("site_id", site_id.map(|id| id.to_string()))
                .scope_opt("parameter_id", parameter_id.map(|id| id.to_string()))
                .count("events_written", count),
        )
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
                .execute_raw(Statement::from_sql_and_values(
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
        let mut staged = ctx
            .db()
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT stream_id, site_id, parameter_id, time, raw_value, \
                        sensor_id, calibration_id, deployment_id \
                 FROM csv_import_staging WHERE import_token = $1 ORDER BY seq",
                [import_token.into()],
            ))
            .await?;

        // The import handler refuses rows targeting a replicate-family stream before staging, and
        // the same rule holds here so no staging row, however it got there, mints replicate
        // indexes onto a family (a reading's index is the source's column position).
        {
            let mut staged_streams: Vec<Uuid> = staged
                .iter()
                .filter_map(|row| row.try_get::<Uuid>("", "stream_id").ok())
                .collect();
            staged_streams.sort_unstable();
            staged_streams.dedup();
            let family_ids: std::collections::HashSet<Uuid> = ctx
                .db()
                .query_all_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT id FROM data_streams \
                     WHERE id = ANY($1) AND metadata -> 'replicates' IS NOT NULL",
                    [staged_streams.into()],
                ))
                .await?
                .iter()
                .filter_map(|row| row.try_get::<Uuid>("", "id").ok())
                .collect();
            if !family_ids.is_empty() {
                let before = staged.len();
                staged.retain(|row| {
                    row.try_get::<Uuid>("", "stream_id")
                        .is_ok_and(|id| !family_ids.contains(&id))
                });
                ctx.log(
                    "warn",
                    "Staged rows targeting replicate-family streams were dropped; family \
                     replicates sync from the source and cannot be written by CSV import",
                    serde_json::json!({ "rows_dropped": before - staged.len() }),
                )
                .await;
            }
        }

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
                    .query_all_raw(Statement::from_sql_and_values(
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
                .query_all_raw(Statement::from_sql_and_values(
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
                collection_event_id: Set(None),
                provenance: Set(None),
                label: Set(None),
                notes: Set(None),
                created_by: Set(None),
                withdrawn_at: Set(None),
                withdrawn_reason: Set(None),
                ingested_at: sea_orm::ActiveValue::NotSet,
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
        // one answer, two or more rows classified spot sharing a slot instant.
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
            .filter(|count| sample_groups::forms_sample(**count))
            .count();

        let total = i32::try_from(models.len()).unwrap_or(i32::MAX);
        ctx.set_progress(0, Some(total)).await;

        // An overwrite replaces the whole replicate set, not the Nth row by the Nth: stored spot
        // replicates beyond the incoming count would survive a positional upsert and keep
        // double-counting the group, so the tail leaves the group before the insert.
        let mut spot_group_sizes: std::collections::HashMap<
            (Uuid, chrono::DateTime<chrono::Utc>),
            i16,
        > = std::collections::HashMap::new();
        if conflict == ConflictMode::Overwrite {
            for m in &models {
                if m.measurement_type.as_ref().as_deref() == Some(sample_groups::SPOT) {
                    *spot_group_sizes
                        .entry((
                            *m.stream_id.as_ref(),
                            m.time.as_ref().with_timezone(&chrono::Utc),
                        ))
                        .or_default() += 1;
                }
            }
        }

        // Phase 1: displace the tail and insert, in one transaction. A crash between the two
        // would otherwise leave the group short of both its old rows and its new ones.
        let mut inserted_so_far = 0usize;
        let (affected_total, corrected) = crate::common::bulk_write::guarded(ctx.db(), async |txn| {
            for ((stream_id, time), count) in &spot_group_sizes {
                displace_spot_tail(txn, *stream_id, *time, *count).await?;
            }

            let mut affected_total = 0usize;
            let mut corrected = 0usize;
            for chunk in models.chunks(CSV_BATCH_SIZE) {
                // A correction corrects the measurement, so it is a decision: recorded before the
                // write, against the values still stored, and only for the rows the write moves
                // (ADR 0008). The count it returns is what the run reports as overwritten.
                if conflict == ConflictMode::Overwrite {
                    let corrections =
                        crate::routes::private::readings::decisions::record_value_corrections(
                            txn,
                            chunk,
                            "csv_import",
                            crate::routes::private::readings::decisions::Origin::Csv,
                        )
                        .await?;
                    corrected += usize::try_from(corrections.rows).unwrap_or(usize::MAX);
                }
                match readings::Entity::insert_many(chunk.to_vec())
                    .on_conflict(readings_on_conflict(conflict))
                    .exec_without_returning(txn)
                    .await
                {
                    Ok(affected) => affected_total += affected as usize,
                    Err(e) => {
                        let msg = e.to_string();
                        if !msg.contains("None of the records") {
                            tracing::warn!(error = %e, "Failed to insert imported readings chunk");
                            return Err(e.into());
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
            Ok((affected_total, corrected))
        })
        .await
        .map_err(as_db_err)?;

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
                crate::routes::private::sensors::calibrations::service::recompose_from_own_curves_guarded(
                    ctx.db(),
                    "TRUE",
                    "r.stream_id = ANY($1) AND r.time >= $2 AND r.time <= $3",
                    vec![
                        stream_ids.into(),
                        sea_orm::prelude::DateTimeWithTimeZone::from(*first).into(),
                        sea_orm::prelude::DateTimeWithTimeZone::from(*last).into(),
                    ],
                )
                .await
                .map_err(as_db_err)?;
            tracing::info!(
                site = %site_name,
                recomposed,
                "CSV overwrite recomposed corrected values from the curves already on the rows"
            );
        }

        let mut touched_visits: Vec<
            crate::routes::private::collection_events::recompute::TouchedEvent,
        > = Vec::new();
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
            let stream_ids_for_events = stream_ids.clone();
            sample_groups::materialise_samples(
                ctx.db(),
                &row_predicate,
                vec![stream_ids.clone().into()],
            )
            .await
            .map_err(as_db_err)?;

            // A CSV import is a person entering visits after the fact: manual collection events.
            crate::routes::private::collection_events::attach::attach_collection_events(
                ctx.db(),
                &row_predicate,
                vec![stream_ids.into()],
                crate::routes::private::collection_events::attach::EventSource::Manual,
            )
            .await
            .map_err(as_db_err)?;

            // The values have landed at their visits; the calculations that read them run without
            // anyone asking (ADR 0007). Read after the attach, which is what gives the rows the
            // events this looks them up by.
            touched_visits = crate::routes::private::collection_events::recompute::touched_events(
                ctx.db(),
                &row_predicate,
                vec![stream_ids_for_events.into()],
            )
            .await
            .map_err(as_db_err)?;
        }

        let (inserted_total, overwritten) = match conflict {
            ConflictMode::Skip => (affected_total, 0),
            // The rows a correction was recorded for are the rows the write moved, so the run
            // reports the same number the decision record holds rather than the staged estimate.
            ConflictMode::Overwrite => (affected_total.saturating_sub(overlapping), corrected),
        };
        tracing::info!(site = %site_name, inserted_total, overwritten, "CSV import inserted readings");

        if inserted_total > 0 || overwritten > 0 {
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

            // An import is a person entering visits after the fact, so the rollups are refreshed
            // from the earliest instant it landed, and a failure there fails the job: a swallowed
            // refresh reports an import as complete while the rollups still serve the old numbers.
            // The window can be long, so episodes are rebuilt by the `alarm_backfill` job.
            let app = crate::common::global_app_state();
            let written =
                tail::Written::new(u64::try_from(inserted_total + overwritten).unwrap_or(u64::MAX))
                    .over(since.zip(latest))
                    .at(param_streams
                        .iter()
                        .map(|(parameter_id, stream_id)| {
                            tail::Slot::paired(site_id, *parameter_id).through(*stream_id)
                        })
                        .collect())
                    .touching(touched_visits);
            tail::run(
                tail::Sink {
                    db: ctx.db(),
                    events: ctx.events(),
                    cache: app.as_ref().map(|a| &a.response_cache),
                },
                &written,
                &tail::Axes {
                    cache: tail::Cache::Sites,
                    refresh: tail::Refresh::Since { fatal: true },
                    announce: true,
                    reconcile_alarms: false,
                    episodes: tail::Episodes::Job,
                    writer: crate::routes::private::collection_events::recompute::Writer::Person,
                },
                "csv_import",
            )
            .await
            .map_err(as_db_err)?;
        }

        // The staging source has served its purpose, drop it (makes this job non-rerunnable).
        ctx.db()
            .execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "DELETE FROM csv_import_staging WHERE import_token = $1",
                [import_token.into()],
            ))
            .await?;

        ctx.report(
            JobReport::new()
                .scope("site_id", site_id.to_string())
                .count("inserted", inserted_total)
                .count("overwritten", overwritten)
                .count("replicate_groups", replicate_groups),
        )
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
        let mut results = Vec::with_capacity(slots.len());
        for (site_id, parameter_id) in slots {
            let moved = reprocess_site_parameter_readings(ctx.db(), site_id, parameter_id)
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
            let moved = reprocess_sensor_readings(ctx.db(), sensor_id)
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

/// Apply a pairing plan: resolve entities, execute pairings, backfill readings, mark the plan
/// `applied`. The status transition is guarded (only a `draft` plan applies), so a re-execution
/// after a lost lease does nothing; not offered as a rerun. Backs the `apply_pairing_plan`
/// operator action.
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
        ctx.report(
            JobReport::new()
                .scope("plan_id", plan_id.to_string())
                .count("streams_paired", result.streams_paired)
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
/// mark the plan `reverted`. The status transition is guarded (only an `applied` plan reverts), so
/// a re-execution after a lost lease does nothing; not offered as a rerun. Backs the
/// `revert_pairing_plan` operator action.
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
            key: "retention_days",
            kind: TunableKind::Integer,
            min: Some(1),
            max: None,
            default: serde_json::json!(self.operator_retention_days),
            help: "How long an operator or metadata job row is kept before the prune removes it.",
        }]
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
        //    index range instead of hashing the whole hypertable; the full-refresh tick runs it
        //    unbounded, which is what covers drift older than that window.
        let since = (!do_full)
            .then(|| chrono::Utc::now() - chrono::Duration::seconds((cadence_seconds * 2) as i64));
        janitor::run_once(db, Some(&ctx), since).await?;

        // 2. Repair corrected readings whose stored value is no longer what their own curves
        //    produce, whichever route moved them apart. Hooks make that repair immediate; this makes
        //    it eventual, so a hook that never fired costs staleness rather than a wrong number.
        //    Refreshed over the span it moved, before the rollups below settle for this tick.
        let mut recomposed = 0u64;
        match crate::routes::private::sensors::calibrations::service::sweep_curve_drift(db).await {
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

        // 4. Report, never repair: readings whose curation columns are not the fold of their
        //    live decisions. Which side is wrong is a decision (a rollback, or a fresh decision),
        //    so a sweep may not pick one.
        let curation_drift =
            match crate::routes::private::readings::decisions::curation_drift_count(db).await {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(error = %e, "Janitor: curation drift count failed");
                    0
                }
            };
        if curation_drift > 0 {
            ctx.log(
                "warn",
                &format!(
                    "{curation_drift} readings disagree with their decision record; nothing was changed"
                ),
                serde_json::json!({}),
            )
            .await;
        }

        // What this tick actually changed, so a run's effect is readable per job rather than only
        // in its logs.
        ctx.report(
            JobReport::new()
                .scope("full_refresh", do_full)
                .count("recomposed", recomposed)
                .count("pruned", pruned)
                .count("curation_drift", curation_drift),
        )
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
                ctx.report(
                    JobReport::new()
                        .count("opened", stats.opened)
                        .count("resolved", stats.resolved),
                )
                .await;
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
        ctx.report(
            JobReport::new()
                .scope("stale_after_seconds", self.stale_after_seconds)
                .count("sync_events_closed", closed),
        )
        .await;
        Ok(closed as i64)
    }
}

/// Close 'running' sync_events older than the staleness threshold; returns the row count.
pub async fn sweep_stale_sync_events(
    db: &sea_orm::DatabaseConnection,
    stale_after_seconds: u64,
) -> Result<u64, DbErr> {
    let res = db
        .execute_raw(sea_orm::Statement::from_sql_and_values(
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

/// Age-based retention for the sync ledgers. sync_events accretes one row per cycle and
/// ingest_receipts one per windowed pass; without pruning both grow forever. Running
/// sync_events rows are never touched (the staleness sweep owns those), and a stream's
/// current state never rests on an old receipt: the source re-asserts its windows.
pub struct SyncLedgerRetention {
    sync_event_retention_days: u32,
    ingest_receipt_retention_days: u32,
}

impl SyncLedgerRetention {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            sync_event_retention_days: config.sync_event_retention_days,
            ingest_receipt_retention_days: config.ingest_receipt_retention_days,
        }
    }
}

#[async_trait]
impl Job for SyncLedgerRetention {
    fn name(&self) -> &'static str {
        "sync_ledger_retention"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(86_400))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let db = ctx.db();
        let mut events_pruned = 0u64;
        if self.sync_event_retention_days > 0 {
            events_pruned = db
                .execute_raw(sea_orm::Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "DELETE FROM sync_events
                     WHERE status <> 'running'
                       AND started_at < NOW() - ($1 || ' days')::interval",
                    [self.sync_event_retention_days.to_string().into()],
                ))
                .await?
                .rows_affected();
        }
        let mut receipts_pruned = 0u64;
        if self.ingest_receipt_retention_days > 0 {
            receipts_pruned = db
                .execute_raw(sea_orm::Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "DELETE FROM ingest_receipts
                     WHERE at < NOW() - ($1 || ' days')::interval",
                    [self.ingest_receipt_retention_days.to_string().into()],
                ))
                .await?
                .rows_affected();
        }
        ctx.report(
            JobReport::new()
                .scope("sync_event_retention_days", self.sync_event_retention_days)
                .scope(
                    "ingest_receipt_retention_days",
                    self.ingest_receipt_retention_days,
                )
                .count("sync_events_pruned", events_pruned)
                .count("ingest_receipts_pruned", receipts_pruned),
        )
        .await;
        Ok((events_pruned + receipts_pruned) as i64)
    }
}

/// Queue a trigger_full_sync for every live, unpaused service with `full_reassert_enabled`. The
/// digest handshake stops a service re-sending unchanged content, which also means routine passes
/// can no longer repair server-side drift (rows changed outside the sync path); the full pass
/// ignores digests and re-asserts everything the source holds.
///
/// What that repairs depends on the source. A reconciled backend declares the window it
/// re-asserts, so its diff applies the corrections. An append-only one (Vaisala, NOMIS) sends no
/// window and the driver ingests with `overwrite` false, so the pass inserts rows missing here and
/// leaves every stored value as it is; correcting those is `resync_streams`, not this.
///
/// Delivery is the normal heartbeat pickup; a service already holding a pending command is not
/// queued twice.
pub struct SyncFullReassert {
    command_expiry_secs: u64,
}

impl SyncFullReassert {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            command_expiry_secs: config.sync_command_expiry_secs,
        }
    }
}

#[async_trait]
impl Job for SyncFullReassert {
    fn name(&self) -> &'static str {
        "sync_full_reassert"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(604_800))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let rows = ctx
            .db()
            .query_all_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "INSERT INTO sync_commands
                     (id, service_id, command, status, created_at, expires_at)
                 SELECT gen_random_uuid(), s.id, 'trigger_full_sync', 'pending', NOW(),
                        NOW() + ($1 || ' seconds')::interval
                 FROM sync_services s
                 WHERE s.paused IS NOT TRUE
                   AND s.full_reassert_enabled
                   AND s.last_heartbeat > NOW() - INTERVAL '1 hour'
                   AND NOT EXISTS (
                       SELECT 1 FROM sync_commands c
                       WHERE c.service_id = s.id
                         AND c.command = 'trigger_full_sync'
                         AND c.status = 'pending'
                         AND c.expires_at > NOW()
                   )
                 RETURNING service_id",
                [self.command_expiry_secs.to_string().into()],
            ))
            .await?;
        let services: Vec<String> = rows
            .iter()
            .filter_map(|r| r.try_get::<Uuid>("", "service_id").ok())
            .map(|id| id.to_string())
            .collect();
        let queued = services.len();
        ctx.report(
            JobReport::new()
                .scope("service_ids", services)
                .count("commands_queued", queued),
        )
        .await;
        Ok(i64::try_from(queued).unwrap_or(i64::MAX))
    }
}

/// Prune Web Push subscriptions for users whose Keycloak account is revoked or disabled.
pub struct PushSubscriptionReconcile {
    interval_seconds: u64,
}

impl PushSubscriptionReconcile {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            interval_seconds: config.identity_reconcile_interval_seconds,
        }
    }
}

#[async_trait]
impl Job for PushSubscriptionReconcile {
    fn name(&self) -> &'static str {
        "identity_reconcile"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(self.interval_seconds.max(1) as i64))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let Some(state) = crate::common::global_app_state() else {
            tracing::debug!("push_subscription_reconcile: no AppState in process; skipping");
            return Ok(0);
        };
        match crate::routes::private::notifications::reconcile::sweep(&state).await {
            Ok(o) => {
                if o.total() > 0 {
                    tracing::info!(
                        revoked = o.revoked,
                        deactivated = o.deactivated,
                        "Push subscription reconciliation: users pruned"
                    );
                }
                ctx.report(
                    JobReport::new()
                        .count("revoked", o.revoked)
                        .count("deactivated", o.deactivated),
                )
                .await;
                Ok(o.total() as i64)
            }
            Err(e) => Err(e),
        }
    }
}

/// Probe each configured notification channel and upsert
/// The channel health heartbeat. Wraps [`health::probe_once`].
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
        let probed =
            crate::routes::private::notifications::health::probe_once(ctx.db(), &state.config)
                .await;
        ctx.report(JobReport::new().count("channels_probed", probed))
            .await;
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

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        use crate::routes::private::notifications::dispatcher;
        let Some(state) = crate::common::global_app_state() else {
            tracing::debug!("dispatch_notifications: no AppState in process; skipping");
            return Ok(0);
        };
        let channels = dispatcher::build_channels(&state.config);
        ctx.report(JobReport::new().count("channels", channels.len()))
            .await;
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

        // The family guard holds here, not only on the HTTP routes: a stored job row is replayed
        // by rerun with its params verbatim, so a route-only guard is bypassed by replaying a row
        // that predates it. 'spot' is what a family already is, and 'declared' realigns readings
        // with the stream's own declaration, which every write path holds at 'spot'.
        if matches!(target.as_str(), "continuous" | "derived") {
            let families =
                crate::routes::private::data_streams::replicates::family_keys_in_retag_scope(
                    ctx.db(),
                    &sensor_ids,
                    &stream_ids,
                    source_system.as_deref(),
                )
                .await
                .map_err(|e| DbErr::Custom(e.to_string()))?;
            crate::routes::private::data_streams::replicates::refuse_family_retag(
                &families, &target,
            )
            .map_err(|e| DbErr::Custom(e.to_string()))?;
        }

        // 'declared' joins each reading to its stream in the rewrite and drops the target
        // parameter; a fixed target compares against $1.
        // The sensor arm also matches by stream ownership: readings ingested before attribution
        // backfill carry sensor_id NULL but belong to the sensor's streams all the same.
        let (scope, mismatch, new_value, update_from) = if declared {
            (
                "(r.sensor_id = ANY($1) OR r.stream_id = ANY($2) \
                  OR r.stream_id IN (SELECT id FROM data_streams WHERE sensor_id = ANY($1)) \
                  OR ($3::text IS NOT NULL AND r.stream_id IN \
                      (SELECT id FROM data_streams WHERE source_system = $3)))",
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
        let touched = crate::common::bulk_write::guarded_mutation(
            ctx.db(),
            Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                &format!(
                    "UPDATE readings r SET measurement_type = {new_value} \
                     {update_from} WHERE {mismatch} AND {scope}"
                ),
                values,
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
        let scope = "EXISTS (SELECT 1 FROM site_parameters sp \
                     WHERE sp.site_id = s.site_id AND sp.parameter_id = s.parameter_id \
                       AND (sp.id = ANY($2) \
                            OR EXISTS (SELECT 1 FROM data_streams ds \
                                       WHERE ds.id = ANY($3) AND ds.site_parameter_id = sp.id)))";

        let skipped = if override_instants {
            0
        } else {
            ctx.db()
                .query_one_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    format!(
                        "SELECT COUNT(*)::bigint AS n FROM samples s \
                         WHERE {scope} AND s.sd_estimator_source = 'sample' \
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
                     WHERE {scope} AND s.sd_estimator IS DISTINCT FROM $1{instant_guard}{window}"
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
                     WHERE {scope} AND s.sd_estimator = $1{instant_guard}"
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

#[cfg(test)]
mod slot_outcome_tests {
    use super::SlotOutcome;
    use sea_orm::DbErr;

    fn slot(n: u32) -> serde_json::Value {
        serde_json::json!({ "site_id": n })
    }

    #[test]
    fn a_failed_slot_is_named_and_the_rest_still_count() {
        let outcome = SlotOutcome::from(vec![
            (slot(1), Ok(4)),
            (slot(2), Err(DbErr::Custom("lock timeout".into()))),
            (slot(3), Ok(6)),
        ]);

        assert_eq!(outcome.succeeded, 2);
        assert_eq!(outcome.readings, 10);
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].0, slot(2));
        assert!(outcome.failed[0].1.contains("lock timeout"));
        assert!(!outcome.all_failed());
    }

    #[test]
    fn every_slot_failing_is_a_failed_run() {
        let outcome = SlotOutcome::from(vec![
            (slot(1), Err(DbErr::Custom("a".into()))),
            (slot(2), Err(DbErr::Custom("b".into()))),
        ]);

        assert_eq!(outcome.readings, 0);
        assert!(outcome.all_failed());
        assert!(outcome.error().to_string().contains('2'));
    }

    #[test]
    fn an_empty_slot_set_is_not_a_failure() {
        let outcome = SlotOutcome::from(Vec::new());

        assert_eq!(outcome.succeeded, 0);
        assert_eq!(outcome.readings, 0);
        assert!(!outcome.all_failed());
    }
}

#[cfg(test)]
mod tunable_validation_tests {
    use super::{AlarmSweep, JanitorRun};
    use crate::routes::private::reprocessing_jobs::job::{Job, TunableKind};

    fn janitor() -> JanitorRun {
        JanitorRun {
            interval_seconds: 300,
            full_refresh_seconds: 3600,
            maintenance_retention_days: 7,
            operator_retention_days: 90,
            maintenance_max_rows: 100_000,
        }
    }

    #[test]
    fn a_misspelt_tunable_is_refused_naming_it() {
        let err = janitor()
            .validate(&serde_json::json!({ "retention_dayz": 7 }))
            .unwrap_err();
        assert!(err.contains("retention_dayz"), "{err}");
        assert!(err.contains("retention_days"), "{err}");
    }

    #[test]
    fn the_janitor_still_takes_its_one_key() {
        assert!(
            janitor()
                .validate(&serde_json::json!({ "retention_days": 7 }))
                .is_ok()
        );
        assert!(janitor().validate(&serde_json::json!({})).is_ok());
        assert!(janitor().validate(&serde_json::Value::Null).is_ok());
        assert!(
            janitor()
                .validate(&serde_json::json!({ "retention_days": 0 }))
                .is_err()
        );
    }

    #[test]
    fn a_job_with_no_tunables_refuses_every_key() {
        let sweep = AlarmSweep {
            interval_seconds: 60,
        };
        assert!(sweep.validate(&serde_json::json!({})).is_ok());
        let err = sweep
            .validate(&serde_json::json!({ "retention_days": 7 }))
            .unwrap_err();
        assert!(err.contains("no tunables"), "{err}");
        assert!(err.contains("retention_days"), "{err}");
    }

    /// Every job accepts a tunables object built from its own declared defaults, and refuses a
    /// value outside a spec's range.
    #[test]
    fn every_job_accepts_its_own_defaults_and_refuses_an_out_of_range_value() {
        let registry = crate::routes::private::reprocessing_jobs::job::build_registry();
        for name in registry.names() {
            let handler = registry.get(name).expect("a listed name is registered");
            let specs = handler.tunables();
            let defaults: serde_json::Map<String, serde_json::Value> = specs
                .iter()
                .map(|s| (s.key.to_string(), s.default.clone()))
                .collect();
            handler
                .validate(&serde_json::Value::Object(defaults))
                .unwrap_or_else(|e| panic!("{name} refuses its own defaults: {e}"));

            for spec in &specs {
                let Some(min) = spec.min else { continue };
                if !matches!(spec.kind, TunableKind::Integer | TunableKind::Duration) {
                    continue;
                }
                let below = serde_json::json!({ spec.key: min - 1 });
                assert!(
                    handler.validate(&below).is_err(),
                    "{name} accepts {} below its minimum",
                    spec.key
                );
            }
        }
    }
}
