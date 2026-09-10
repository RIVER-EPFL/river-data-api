//! The alarm sweeps: the live reconcile against the current breach set, the historical episode
//! rebuild, and the two jobs that drive them.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, FromQueryResult, Statement};
use std::collections::HashSet;
use uuid::Uuid;

use super::service::{
    EpisodeRow, ExtentRow, SlotRow, fetch_active_alarm_rows, fetch_episodes, resolve_threshold,
    severity_case,
};
use crate::common::{AppEvent, EventSender};
use crate::config::Config;
use crate::error::AppResult;
use crate::routes::private::reprocessing_jobs::job::Job;
use crate::routes::private::reprocessing_jobs::jobs::{
    SlotOutcome, optional_datetime, optional_uuid, uuid_pair_array,
};
use crate::routes::private::reprocessing_jobs::lifecycle::{JobContext, JobReport};
use crate::routes::private::reprocessing_jobs::schedule::Schedule;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

tokio::task_local! {
    /// Whether the request in flight has asked for a global reconcile. A CRUD batch runs the
    /// single-row hooks once per row, so the hook records the debt here and the request pays it
    /// once, instead of running the same global pass a hundred times.
    pub(super) static RECONCILE_OWED: Arc<AtomicBool>;
}

/// Record a global reconcile against the request in flight. False where there is no request, a
/// background job or a test, which reconciles on the spot instead.
pub(super) fn record_owed() -> bool {
    RECONCILE_OWED
        .try_with(|owed| owed.store(true, Ordering::Relaxed))
        .is_ok()
}
#[derive(Debug, Default, Clone, Copy)]
pub struct SweepStats {
    pub opened: usize,
    pub updated: usize,
    pub resolved: usize,
}

/// One reconciliation tick. Idempotent: safe to call repeatedly (the partial unique index on open
/// events makes open-or-update a no-op when nothing changed).
pub async fn evaluate_alarm_events<C: ConnectionTrait>(db: &C) -> AppResult<SweepStats> {
    reconcile(db, None).await
}

/// Scoped reconcile for just the given `(site_id, parameter_id)` slots, the event-driven entry
/// point (ingest / threshold / config change). Only opens/updates/resolves events within these
/// slots; alarms outside them are never touched (so it's safe to fire on a partial change).
pub async fn reconcile_open_alarms<C: ConnectionTrait>(
    db: &C,
    slots: &[(Uuid, Uuid)],
) -> AppResult<SweepStats> {
    reconcile(db, Some(slots)).await
}

/// Event-driven call sites use this: reconcile the given slots and emit an `AlarmStateChanged` SSE
/// if anything opened or resolved (mirroring the periodic tick). Never fails the caller, it logs
/// and swallows errors, so wiring it into a write/config path can never break that path. The
/// periodic backstop still reconciles everything regardless.
pub async fn reconcile_and_notify<C: ConnectionTrait>(
    db: &C,
    events: &EventSender,
    slots: &[(Uuid, Uuid)],
) {
    if slots.is_empty() {
        return;
    }
    match reconcile_open_alarms(db, slots).await {
        Ok(stats) => {
            if stats.opened > 0 || stats.resolved > 0 {
                let _ = events.send(AppEvent::AlarmStateChanged {
                    opened: stats.opened,
                    resolved: stats.resolved,
                });
            }
        }
        Err(e) => tracing::warn!(error = %e, "scoped alarm reconcile failed"),
    }
}

/// Global variant of [`reconcile_and_notify`] for background jobs that change values across many
/// slots (derived recompute, calibration/deployment reprocess) where enumerating the exact affected
/// slots isn't worth it. Reconciles every active slot, cheap (O(active slots) index lookups), and
/// emits SSE on change. Error-safe.
pub async fn reconcile_all_and_notify<C: ConnectionTrait>(db: &C, events: &EventSender) {
    match evaluate_alarm_events(db).await {
        Ok(stats) => {
            if stats.opened > 0 || stats.resolved > 0 {
                let _ = events.send(AppEvent::AlarmStateChanged {
                    opened: stats.opened,
                    resolved: stats.resolved,
                });
            }
        }
        Err(e) => tracing::warn!(error = %e, "global alarm reconcile failed"),
    }
}

/// [`reconcile_all_and_notify`] for a CrudCrate operation hook, which is handed the transaction the
/// write runs in. Inside a request it records the debt and [`coalesce_reconcile`] pays it once;
/// anywhere else it reconciles on the spot. Uses the process-global event sender; a missing sender
/// (some unit tests) just skips the SSE. Never returns an error, a failed reconcile must not fail
/// the CRUD operation that triggered it.
pub async fn reconcile_all_from_hook<C: ConnectionTrait>(db: &C) {
    if record_owed() {
        return;
    }
    reconcile_all_now(db).await;
}

pub(super) async fn reconcile_all_now<C: ConnectionTrait>(db: &C) {
    match crate::common::global_event_sender() {
        Some(events) => reconcile_all_and_notify(db, &events).await,
        None => {
            if let Err(e) = evaluate_alarm_events(db).await {
                tracing::warn!(error = %e, "global alarm reconcile failed");
            }
        }
    }
}

/// One reconciliation tick. `slots = None` reconciles every active slot (backstop); `slots = Some`
/// restricts every step to those slots. Idempotent: the partial unique index on open events makes
/// open-or-update a no-op when nothing changed.
async fn reconcile<C: ConnectionTrait>(
    db: &C,
    slots: Option<&[(Uuid, Uuid)]>,
) -> AppResult<SweepStats> {
    if matches!(slots, Some(s) if s.is_empty()) {
        return Ok(SweepStats::default());
    }

    // Sensor (continuous) and grab (spot) series are reconciled independently: each cadence
    // has its own latest reading, its own open event per slot, and its own resolution.
    let mut stats = SweepStats::default();
    for spot in [false, true] {
        let s = reconcile_cadence(db, slots, spot).await?;
        stats.opened += s.opened;
        stats.updated += s.updated;
        stats.resolved += s.resolved;
    }
    Ok(stats)
}

/// The slot an open alarm event stands on.
#[derive(FromQueryResult)]
struct OpenAlarmSlot {
    site_id: Uuid,
    parameter_id: Uuid,
}

async fn reconcile_cadence<C: ConnectionTrait>(
    db: &C,
    slots: Option<&[(Uuid, Uuid)]>,
    spot: bool,
) -> AppResult<SweepStats> {
    let cadence = super::service::cadence_label(spot);

    let breaches = fetch_active_alarm_rows(
        db,
        &crate::common::authz::AccessScope::Unrestricted,
        slots,
        spot,
    )
    .await?;

    // Pairs that already have an open event (within scope), so we can count opened vs updated.
    let (open_keys_sql, open_keys_values): (String, Vec<sea_orm::Value>) = match slots {
        Some(s) => {
            let pairs: Vec<String> = (0..s.len())
                .map(|i| format!("(${},${})", i * 2 + 1, i * 2 + 2))
                .collect();
            let vals = s
                .iter()
                .flat_map(|(a, b)| [(*a).into(), (*b).into()])
                .collect();
            (
                format!(
                    "SELECT site_id, parameter_id FROM alarm_events \
                     WHERE resolved_at IS NULL AND measurement_type = '{cadence}' \
                       AND (site_id, parameter_id) IN ({})",
                    pairs.join(",")
                ),
                vals,
            )
        }
        None => (
            format!(
                "SELECT site_id, parameter_id FROM alarm_events \
                 WHERE resolved_at IS NULL AND measurement_type = '{cadence}'"
            ),
            Vec::new(),
        ),
    };
    let mut open_keys: HashSet<(Uuid, Uuid)> = HashSet::new();
    for row in db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &open_keys_sql,
            open_keys_values,
        ))
        .await?
    {
        let slot = OpenAlarmSlot::from_query_result(&row, "")?;
        open_keys.insert((slot.site_id, slot.parameter_id));
    }

    let mut stats = SweepStats::default();

    // Open-or-update each current breach.
    for b in &breaches {
        let is_new = !open_keys.contains(&(b.site_id, b.parameter_id));
        db.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO alarm_events \
                (site_id, parameter_id, measurement_type, severity, max_severity, started_at, value_at_start, last_seen_at, last_value) \
             VALUES ($1, $2, $6, $3, $3, $4, $5, $4, $5) \
             ON CONFLICT (site_id, parameter_id, measurement_type) WHERE resolved_at IS NULL \
             DO UPDATE SET severity = EXCLUDED.severity, \
                           max_severity = GREATEST(alarm_events.max_severity, EXCLUDED.severity), \
                           last_seen_at = EXCLUDED.last_seen_at, \
                           last_value = EXCLUDED.last_value, \
                           updated_at = NOW()",
            [
                b.site_id.into(),
                b.parameter_id.into(),
                b.severity.into(),
                b.time.into(),
                b.current_value.into(),
                cadence.into(),
            ],
        ))
        .await?;
        if is_new {
            stats.opened += 1;
        } else {
            stats.updated += 1;
        }
    }

    // Resolve open events no longer in the current breach set; stamp the latest reading as the
    // resolving value. When scoped, restrict to the scoped slots so events outside this trigger are
    // never resolved. Empty breach set (within scope) → resolve everything still open (in scope).
    let keep: Vec<(Uuid, Uuid)> = breaches
        .iter()
        .map(|b| (b.site_id, b.parameter_id))
        .collect();
    let mut values: Vec<sea_orm::Value> = Vec::new();
    let mut next = 1usize;

    let scope_clause = if let Some(s) = slots {
        let mut pairs = Vec::with_capacity(s.len());
        for (site, param) in s {
            pairs.push(format!("(${},${})", next, next + 1));
            values.push((*site).into());
            values.push((*param).into());
            next += 2;
        }
        format!(
            " AND (ae.site_id, ae.parameter_id) IN ({})",
            pairs.join(",")
        )
    } else {
        String::new()
    };

    let not_in_clause = if keep.is_empty() {
        String::new()
    } else {
        let mut pairs = Vec::with_capacity(keep.len());
        for (site, param) in &keep {
            pairs.push(format!("(${},${})", next, next + 1));
            values.push((*site).into());
            values.push((*param).into());
            next += 2;
        }
        format!(
            " AND (ae.site_id, ae.parameter_id) NOT IN ({})",
            pairs.join(",")
        )
    };
    // The latest served value under the same per-cadence rule the breach set uses, wrapped to a
    // single column for the scalar assignment.
    let latest = super::service::latest_served_sql(spot, "ae.site_id", "ae.parameter_id");
    let resolve_sql = format!(
        "UPDATE alarm_events ae \
         SET resolved_at = NOW(), \
             updated_at = NOW(), \
             resolved_value = (SELECT lv.value FROM ({latest}) lv) \
         WHERE ae.resolved_at IS NULL AND ae.measurement_type = '{cadence}'{scope_clause}{not_in_clause}"
    );
    let resolved = db
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &resolve_sql,
            values,
        ))
        .await?
        .rows_affected() as usize;
    stats.resolved = resolved;

    Ok(stats)
}
/// Reconstruct resolved breach episodes for one slot over `[start, end]` and persist them
/// idempotently. Returns the number of episode rows written.
pub async fn evaluate_alarm_episodes(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<i64, sea_orm::DbErr> {
    let Some(threshold) = resolve_threshold(db, site_id, parameter_id).await? else {
        return Ok(0);
    };
    if threshold.is_disabled() {
        // No bounds → nothing can breach. Leave any pre-existing history untouched; disabling a
        // parameter shouldn't erase episodes that genuinely occurred before it was disabled.
        return Ok(0);
    }

    let sev_case = severity_case(
        "v",
        "$5::double precision",
        "$6::double precision",
        "$7::double precision",
        "$8::double precision",
    );

    // Sensor and grab series form separate episode streams: a grab breach must not be
    // "resolved" by the next in-range sonde point (or vice versa).
    let mut written = 0i64;
    let mut all_episodes: Vec<(&'static str, Vec<EpisodeRow>)> = Vec::new();
    for spot in [false, true] {
        let episodes = fetch_episodes(
            db,
            site_id,
            parameter_id,
            start,
            end,
            &threshold,
            &sev_case,
            spot,
        )
        .await?;
        all_episodes.push((super::service::cadence_label(spot), episodes));
    }

    // Idempotent: clear the resolved episodes previously written for this slot+window, then reinsert
    // the freshly computed set. Open rows (`resolved_at IS NULL`) are owned by the sweeper and left
    // alone.
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "DELETE FROM alarm_events \
         WHERE site_id = $1 AND parameter_id = $2 AND resolved_at IS NOT NULL \
           AND started_at >= $3 AND started_at <= $4",
        [
            site_id.into(),
            parameter_id.into(),
            start.into(),
            end.into(),
        ],
    ))
    .await?;

    for (cadence, episodes) in &all_episodes {
        for ep in episodes {
            db.execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "INSERT INTO alarm_events \
                    (site_id, parameter_id, measurement_type, severity, max_severity, started_at, \
                     value_at_start, last_seen_at, last_value, resolved_at, resolved_value) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                [
                    site_id.into(),
                    parameter_id.into(),
                    (*cadence).into(),
                    ep.severity.into(),
                    ep.max_severity.into(),
                    ep.started_at.with_timezone(&Utc).into(),
                    ep.value_at_start.into(),
                    ep.last_seen_at.with_timezone(&Utc).into(),
                    ep.last_value.into(),
                    ep.resolved_at.map(|t| t.with_timezone(&Utc)).into(),
                    ep.resolved_value.into(),
                ],
            ))
            .await?;
            written += 1;
        }
    }

    Ok(written)
}
/// Rebuild resolved alarm episodes across every active slot matching the optional `site_id` /
/// `parameter_id` filter. When `start`/`end` are omitted they default per-slot to the slot's reading
/// range (`MIN`/`MAX(time)`). Returns the total number of episode rows written. Slot-level failures
/// are logged and skipped so one bad slot can't abort the whole rebuild.
pub async fn rebuild_alarm_events(
    db: &DatabaseConnection,
    site_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
) -> Result<i64, sea_orm::DbErr> {
    let mut conditions = vec!["sp.is_active = true".to_string()];
    let mut values: Vec<sea_orm::Value> = Vec::new();
    if let Some(s) = site_id {
        values.push(s.into());
        conditions.push(format!("sp.site_id = ${}", values.len()));
    }
    if let Some(p) = parameter_id {
        values.push(p.into());
        conditions.push(format!("sp.parameter_id = ${}", values.len()));
    }
    let slot_sql = format!(
        "SELECT DISTINCT sp.site_id, sp.parameter_id FROM site_parameters sp WHERE {}",
        conditions.join(" AND ")
    );
    let slots: Vec<(Uuid, Uuid)> = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &slot_sql,
            values,
        ))
        .await?
        .iter()
        .map(|r| SlotRow::from_query_result(r, ""))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        // A reading with no slot has no thresholds to evaluate against.
        .filter_map(|r| Some((r.site_id?, r.parameter_id?)))
        .collect();

    let mut total = 0i64;
    for (s, p) in slots {
        let (slot_start, slot_end) = if let (Some(a), Some(b)) = (start, end) {
            (a, b)
        } else {
            let row = db
                .query_one_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT MIN(time) AS lo, MAX(time) AS hi FROM readings \
                     WHERE site_id = $1 AND parameter_id = $2",
                    [s.into(), p.into()],
                ))
                .await?;
            // MIN/MAX over an empty slot are NULL, which is the "no readings" case below.
            let extent = row
                .map(|r| ExtentRow::from_query_result(&r, ""))
                .transpose()?;
            let lo = extent.as_ref().and_then(|e| e.lo);
            let hi = extent.as_ref().and_then(|e| e.hi);
            match (start.or(lo), end.or(hi)) {
                (Some(a), Some(b)) => (a, b),
                _ => continue, // no readings for this slot
            }
        };

        match evaluate_alarm_episodes(db, s, p, slot_start, slot_end).await {
            Ok(n) => total += n,
            Err(e) => tracing::warn!(
                error = %e,
                site_id = %s,
                parameter_id = %p,
                "rebuild_alarm_events: slot failed"
            ),
        }
    }

    Ok(total)
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
                let written =
                    evaluate_alarm_episodes(ctx.db(), *site_id, *parameter_id, start, end).await;
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
        let count = rebuild_alarm_events(ctx.db(), site_id, parameter_id, start, end).await?;
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
/// Reconcile persisted `alarm_events` against the current breach set (open/update/resolve), then
/// emit an `AlarmStateChanged` SSE on change, the alarm-sweeper backstop. Wraps
/// [`sweeper::evaluate_alarm_events`] + the same SSE the old `sweeper::periodic` emitted.
pub struct AlarmSweep {
    pub(crate) interval_seconds: u64,
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
        match evaluate_alarm_events(ctx.db()).await {
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

#[cfg(test)]
#[path = "tests/flows.rs"]
mod tests;
