//! The machinery's own `Job` implementations and the helpers every job body reads its inputs with.
//!
//! Each concrete job lives with the component whose tables it moves; what stays here is the pair
//! that belongs to no component (the rollup refresh and the janitor) and the shared `params`
//! readers.

use std::time::Duration;

use async_trait::async_trait;
use sea_orm::sea_query;
use sea_orm::{DbErr, Statement};
use uuid::Uuid;

use super::service::{Job, JobContext, JobReport, Schedule, TunableKind, TunableSpec};
use crate::config::Config;

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
pub(crate) fn merge_origin(
    params: &serde_json::Value,
) -> crate::routes::private::readings::models::Origin {
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

    /// The closing line a walk writes: what it moved, over how many of the things it walked, and
    /// how many it could not. `noun` is the plural of what was walked, "slots" or "instruments".
    #[must_use]
    pub fn line(&self, noun: &str) -> String {
        let mut line = format!(
            "Moved {} readings across {} {noun}",
            self.readings, self.succeeded
        );
        if !self.failed.is_empty() {
            line.push_str(&format!(", {} failed", self.failed.len()));
        }
        line
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

/// Which of a janitor tick's steps failed and why, in the order they ran. A tick with any failed
/// step is a failed run, so the retry and the job_failed notice reach a sweep failing every tick.
pub struct JanitorOutcome {
    pub failed: Vec<(&'static str, String)>,
}

/// The outcome of a tick whose every step was attempted, from each step's name and result.
pub fn janitor_outcome(
    steps: impl IntoIterator<Item = (&'static str, Result<(), String>)>,
) -> JanitorOutcome {
    JanitorOutcome {
        failed: steps
            .into_iter()
            .filter_map(|(step, result)| result.err().map(|e| (step, e)))
            .collect(),
    }
}

impl JanitorOutcome {
    /// The failed steps on the report's scope, an empty list for a clean tick.
    #[must_use]
    pub fn report_into(&self, report: JobReport) -> JobReport {
        report.scope(
            "failed_steps",
            self.failed
                .iter()
                .map(|(step, error)| serde_json::json!({ "step": step, "error": error }))
                .collect::<Vec<_>>(),
        )
    }

    /// `value` when every step succeeded, otherwise an error naming each failed step.
    pub fn result(&self, value: i64) -> Result<i64, DbErr> {
        if self.failed.is_empty() {
            return Ok(value);
        }
        let steps = self
            .failed
            .iter()
            .map(|(step, error)| format!("{step}: {error}"))
            .collect::<Vec<_>>()
            .join("; ");
        Err(DbErr::Custom(format!("janitor steps failed: {steps}")))
    }
}

/// Parse an array of `[site_id, parameter_id]` UUID pairs under `key`. Each element is a two-string
/// array; malformed elements are skipped.
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

/// Refresh the rollups over the window a caller states, so a change it already committed is
/// visible before the hourly policy would carry it: `from` and `until` for a range, `since` for
/// everything after an instant. A window is required; whole-history repair belongs to the
/// policies, which start at NULL and rematerialise every bucket each hour.
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
            _ => {
                return Err(DbErr::Custom(
                    "refresh_aggregates needs a window: from and until, or since".into(),
                ));
            }
        };
        // A refresh that could not run must fail the job: reporting `completed` while the rollups
        // still serve the old numbers is the failure this job exists to make visible.
        let outcome = tokio::time::timeout(
            Duration::from_secs(600),
            crate::common::aggregates::refresh(ctx.db(), window),
        )
        .await;
        match outcome {
            Ok(Ok(report)) => {
                ctx.info(&report.line()).await;
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

/// A built query as the statement the connection takes.
pub(crate) fn build(query: &sea_query::SelectStatement) -> Statement {
    let (sql, values) = query.build(sea_query::PostgresQueryBuilder);
    Statement::from_sql_and_values(sea_orm::DatabaseBackend::Postgres, sql, values)
}

// ── Recurring Services ───────────────────────────────────────────────────────────────────────────
//
// The background loops are `Job` impls the DB-backed scheduler enqueues on cadence (so exactly one
// replica fires each tick). Each `run` calls its loop body once. Each `default_schedule` returns the cadence from `Config` so the
// scheduler can seed a `schedules` row on first start; the seconds are captured at registry-build
// time. Services that need config / shared in-process services beyond `db`+`params` read the
// process-global `AppState` (`crate::common::global_app_state`), the same set-once handle pattern
// the CrudCrate hooks use for the event sender.

/// Fill missing derived readings, recompose values whose curve coefficients moved, and prune
/// abandoned chunked uploads and old tracked-job rows. The rollups are not its business: they serve their head from the raw rows and
/// each policy rematerialises the history hourly, so what this job refreshes is only the span its
/// own writes moved.
pub struct JanitorRun {
    /// Fallback cadence for the full-refresh decision, used only when the run carries no
    /// scheduler-stamped `interval_seconds` (`run_now`). The `schedules` row is the authority.
    interval_seconds: u64,
    /// How often one tick runs the derived gap scan unbounded instead of over twice the cadence.
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
        let (gaps, gap_fill) = match janitor::run_once(db, Some(&ctx), since).await {
            Ok(gaps) => (gaps, Ok(())),
            Err(e) => {
                tracing::warn!(error = %e, "Janitor: derived gap fill failed");
                (janitor::GapFill::default(), Err(e.to_string()))
            }
        };

        // 2. Repair corrected readings whose stored value is no longer what their own curves
        //    produce, whichever route moved them apart. Hooks make that repair immediate; this makes
        //    it eventual, so a hook that never fired costs staleness rather than a wrong number.
        //    Refreshed over the span it moved, before the rollups below settle for this tick.
        let drift = recompose_curve_drift(db, &ctx).await;

        // 3. Abandoned chunked uploads. An upload that stops part-way leaves its text in
        // `csv_import_chunks` and nothing else deletes it.
        let (sessions_pruned, import_session_prune) =
            match crate::routes::private::readings::service::prune_import_sessions(db).await {
                Ok(n) => (n, Ok(())),
                Err(e) => {
                    tracing::warn!(error = %e, "Janitor: pruning abandoned import sessions failed");
                    (0, Err(e.to_string()))
                }
            };

        // Staged CSV rows no import will read: a job whose body never returned took nothing.
        let (staging_pruned, import_staging_prune) =
            match crate::routes::private::readings::flows::prune_orphaned_staging(db).await {
                Ok(n) => (n, Ok(())),
                Err(e) => {
                    tracing::warn!(error = %e, "Janitor: pruning orphaned import staging failed");
                    (0, Err(e.to_string()))
                }
            };

        // 4. Tiered tracked-job retention (cheap deletes; idempotent to run every tick).
        let pruned = janitor::prune_tracked_jobs(
            db,
            self.maintenance_retention_days,
            operator_retention_days,
            self.maintenance_max_rows,
        )
        .await;

        let outcome = janitor_outcome([
            ("gap_fill", gap_fill),
            ("curve_drift", drift.sweep),
            ("curve_drift_refresh", drift.refresh),
            ("curve_drift_recompute", drift.recompute),
            ("curve_drift_derived", drift.derived),
            ("import_session_prune", import_session_prune),
            ("import_staging_prune", import_staging_prune),
            ("job_prune", pruned.result()),
        ]);

        // What this tick actually changed, so a run's effect is readable per job rather than only
        // in its logs.
        ctx.report(
            outcome.report_into(
                gaps.report_into(JobReport::new())
                    .scope("full_scan", do_full)
                    .count("recomposed", drift.moved)
                    .count("derived_recomputes_queued", drift.derived_queued)
                    .count("import_sessions_pruned", sessions_pruned)
                    .count("import_staging_pruned", staging_pruned)
                    .count("pruned", pruned.deleted),
            ),
        )
        .await;
        outcome.result(pruned.deleted as i64)
    }
}

/// What the janitor's curve-drift step moved, and the result of each of its four parts.
struct DriftSteps {
    moved: u64,
    derived_queued: usize,
    sweep: Result<(), String>,
    refresh: Result<(), String>,
    recompute: Result<(), String>,
    derived: Result<(), String>,
}

/// Recompose drifted curve values, refresh the rollups over the span they moved, and enqueue the
/// visits and the stream-arm calculations whose inputs moved. A part not reached because nothing
/// moved reports `Ok`.
async fn recompose_curve_drift(db: &sea_orm::DatabaseConnection, ctx: &JobContext) -> DriftSteps {
    let mut steps = DriftSteps {
        moved: 0,
        derived_queued: 0,
        sweep: Ok(()),
        refresh: Ok(()),
        recompute: Ok(()),
        derived: Ok(()),
    };
    let drift = match crate::routes::private::sensor_calibrations::service::sweep_curve_drift(
        db,
        Some(ctx.job_id()),
    )
    .await
    {
        Ok(drift) => drift,
        Err(e) => {
            tracing::warn!(error = %e, "Janitor: curve drift sweep failed");
            steps.sweep = Err(e.to_string());
            return steps;
        }
    };
    if drift.moved == 0 {
        return steps;
    }
    steps.moved = drift.moved;
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
    if let Some((lo, hi)) = drift.span {
        match crate::common::aggregates::refresh(
            db,
            crate::common::aggregates::Window::Range(lo, hi),
        )
        .await
        {
            Ok(report) => ctx.info(&report.line()).await,
            Err(e) => {
                tracing::warn!(error = %e, "Janitor: refresh after curve drift failed");
                steps.refresh = Err(e.to_string());
            }
        }
    }
    // A rewritten spot value is an input somebody's calculation read, so the visits it moved
    // recompute in dependency order rather than being left stale (Q108).
    let enqueued = match crate::routes::private::collection_events::flows::events_from_pairs(
        db,
        &drift.touched,
    )
    .await
    {
        Ok(events) => {
            crate::routes::private::collection_events::flows::enqueue_for(
                db,
                &events,
                "janitor",
                crate::routes::private::collection_events::flows::Writer::Person,
            )
            .await
        }
        Err(e) => Err(e),
    };
    if let Err(e) = enqueued {
        tracing::warn!(error = %e, "Janitor: recompute after curve drift failed");
        steps.recompute = Err(e.to_string());
    }
    // A rewritten continuous value is an input a stream calculation read at that instant, so each
    // slot it moved recomputes over its span as a job of its own under this run (Q108).
    match enqueue_drift_derived(db, ctx, &drift.stream_slots).await {
        Ok(queued) => steps.derived_queued = queued,
        Err(e) => {
            tracing::warn!(error = %e, "Janitor: derived recompute after curve drift failed");
            steps.derived = Err(e.to_string());
        }
    }
    steps
}

/// Enqueue a windowed `derived_recompute` for each drifted slot at a site with a stream-arm
/// calculation, returning how many were queued.
async fn enqueue_drift_derived(
    db: &sea_orm::DatabaseConnection,
    ctx: &JobContext,
    slots: &[crate::routes::private::sensor_calibrations::service::DriftSlot],
) -> Result<usize, DbErr> {
    use crate::routes::private::derived_parameters::flows::site_has_active_derived;
    let mut queued = 0;
    for slot in slots {
        if !site_has_active_derived(db, slot.site_id).await? {
            continue;
        }
        crate::routes::private::reprocessing_jobs::service::enqueue_child(
            db,
            "derived_recompute",
            &slot.recompute_params(),
            Some(ctx.job_id()),
        )
        .await?;
        queued += 1;
    }
    Ok(queued)
}

#[cfg(test)]
#[path = "tests/tunable_validation.rs"]
mod tunable_validation_tests;

#[cfg(test)]
#[path = "tests/slot_outcome.rs"]
mod slot_outcome_tests;

#[cfg(test)]
#[path = "tests/janitor_outcome.rs"]
mod janitor_outcome_tests;
