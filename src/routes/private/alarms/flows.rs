//! The alarm sweeps: the live reconcile against the current breach set, the historical episode
//! rebuild, and the two jobs that drive them.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use crudcrate::{UpsertStatus, upsert};
use sea_orm::sea_query::{Alias, Expr, Query as SeaQuery, QueryStatementBuilder, SimpleExpr};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait,
    QueryFilter, QuerySelect, Set, TransactionSession, TransactionTrait,
};
use uuid::Uuid;

use super::models::alarm_event;
use super::models::alarm_event::AlarmEvent;
use super::service::{
    EpisodeRow, ExtentRow, SlotRow, fetch_active_alarm_rows, fetch_episodes, resolve_threshold,
    severity_case,
};
use crate::common::{AppEvent, EventSender};
use crate::config::Config;
use crate::error::AppResult;
use crate::routes::private::readings::models as readings;
use crate::routes::private::reprocessing_jobs::flows::{
    SlotOutcome, optional_datetime, optional_uuid, uuid_pair_array,
};
use crate::routes::private::reprocessing_jobs::service::Job;
use crate::routes::private::reprocessing_jobs::service::{JobContext, JobReport, Schedule};
use crate::routes::private::site_parameters::models as site_parameters;
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
/// The two things that raise an episode. `kind` on the row says which, and the open-unique index
/// carries it, so one slot can hold one of each at a time.
pub const KIND_THRESHOLD: &str = "threshold";
pub const KIND_INSTRUMENT_RANGE: &str = "instrument_range";

#[derive(Debug, Default, Clone, Copy)]
pub struct SweepStats {
    pub opened: usize,
    pub updated: usize,
    pub resolved: usize,
}

/// One reconciliation tick. Idempotent: safe to call repeatedly (the partial unique index on open
/// events makes open-or-update a no-op when nothing changed).
pub async fn evaluate_alarm_events<C: ConnectionTrait + TransactionTrait>(
    db: &C,
) -> AppResult<SweepStats> {
    reconcile(db, None).await
}

/// Scoped reconcile for just the given `(site_id, parameter_id)` slots, the event-driven entry
/// point (ingest / threshold / config change). Only opens/updates/resolves events within these
/// slots; alarms outside them are never touched (so it's safe to fire on a partial change).
pub async fn reconcile_open_alarms<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    slots: &[(Uuid, Uuid)],
) -> AppResult<SweepStats> {
    reconcile(db, Some(slots)).await
}

/// Event-driven call sites use this: reconcile the given slots and emit an `AlarmStateChanged` SSE
/// if anything opened or resolved (mirroring the periodic tick). Never fails the caller, it logs
/// and swallows errors, so wiring it into a write/config path can never break that path. The
/// periodic backstop still reconciles everything regardless.
pub async fn reconcile_and_notify<C: ConnectionTrait + TransactionTrait>(
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
pub async fn reconcile_all_and_notify<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    events: &EventSender,
) {
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
pub async fn reconcile_all_from_hook<C: ConnectionTrait + TransactionTrait>(db: &C) {
    if record_owed() {
        return;
    }
    reconcile_all_now(db).await;
}

pub(super) async fn reconcile_all_now<C: ConnectionTrait + TransactionTrait>(db: &C) {
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
async fn reconcile<C: ConnectionTrait + TransactionTrait>(
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
        for s in [
            reconcile_cadence(db, slots, spot).await?,
            reconcile_instrument_range(db, slots, spot).await?,
        ] {
            stats.opened += s.opened;
            stats.updated += s.updated;
            stats.resolved += s.resolved;
        }
    }
    Ok(stats)
}

async fn reconcile_cadence<C: ConnectionTrait + TransactionTrait>(
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

    // Open-or-update each current breach as one registration of the open episode: the status
    // says whether it opened or joined one. `max_severity` is entity-managed, so the registration
    // leaves it and the sweeper advances it here, in the same transaction.
    let mut stats = SweepStats::default();
    let txn = db.begin().await?;
    for b in &breaches {
        let sent = alarm_event::ActiveModel {
            id: Set(Uuid::new_v4()),
            site_id: Set(b.site_id),
            parameter_id: Set(b.parameter_id),
            measurement_type: Set(cadence.to_string()),
            kind: Set(KIND_THRESHOLD.to_string()),
            severity: Set(b.severity),
            max_severity: Set(b.severity),
            started_at: Set(b.time.with_timezone(&Utc)),
            value_at_start: Set(b.current_value),
            last_seen_at: Set(b.time.with_timezone(&Utc)),
            last_value: Set(b.current_value),
            ..Default::default()
        };
        let (episode, status) = upsert::<AlarmEvent, _>(&txn, sent).await?;
        match status {
            UpsertStatus::Created => stats.opened += 1,
            UpsertStatus::Updated | UpsertStatus::Unchanged => stats.updated += 1,
        }
        if b.severity > episode.max_severity {
            alarm_event::ActiveModel {
                id: Set(episode.id),
                max_severity: Set(b.severity),
                ..Default::default()
            }
            .update(&txn)
            .await?;
        }
    }
    txn.commit().await?;

    // Resolve open events no longer in the current breach set; stamp the latest reading as the
    // resolving value. When scoped, restrict to the scoped slots so events outside this trigger are
    // never resolved. Empty breach set (within scope) → resolve everything still open (in scope).
    let keep: Vec<(Uuid, Uuid)> = breaches
        .iter()
        .map(|b| (b.site_id, b.parameter_id))
        .collect();
    stats.resolved = resolve_absent(db, slots, &keep, cadence, KIND_THRESHOLD, spot).await?;

    Ok(stats)
}

/// Stamp every open episode of this cadence and kind that is no longer in `keep` as resolved,
/// carrying the latest served value as the resolving one. Scoped runs resolve only within their
/// own slots, so a trigger about one slot never closes an episode elsewhere.
async fn resolve_absent<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    slots: Option<&[(Uuid, Uuid)]>,
    keep: &[(Uuid, Uuid)],
    cadence: &str,
    kind: &str,
    spot: bool,
) -> AppResult<usize> {
    use sea_orm::sea_query::ExprTrait;
    let slot_tuple = Expr::tuple([
        Expr::col(alarm_event::Column::SiteId),
        Expr::col(alarm_event::Column::ParameterId),
    ]);

    // The latest served value under the same per-cadence rule the breach set uses, wrapped to a
    // single column for the scalar assignment.
    let latest = super::service::latest_served_query(
        spot,
        Expr::col((alarm_event::Entity, alarm_event::Column::SiteId)),
        Expr::col((alarm_event::Entity, alarm_event::Column::ParameterId)),
    );
    let lv = Alias::new("lv");
    let resolving_value = SeaQuery::select()
        .column((lv.clone(), Alias::new("value")))
        .from_subquery(latest, lv)
        .take();

    let mut update = alarm_event::Entity::update_many()
        .col_expr(alarm_event::Column::ResolvedAt, Expr::current_timestamp())
        .col_expr(alarm_event::Column::UpdatedAt, Expr::current_timestamp())
        .col_expr(
            alarm_event::Column::ResolvedValue,
            SimpleExpr::SubQuery(None, Box::new(resolving_value.into_sub_query_statement())),
        )
        .filter(alarm_event::Column::ResolvedAt.is_null())
        .filter(alarm_event::Column::MeasurementType.eq(cadence))
        .filter(alarm_event::Column::Kind.eq(kind));
    if let Some(s) = slots {
        update = update.filter(slot_tuple.clone().in_tuples(s.iter().copied()));
    }
    if !keep.is_empty() {
        update = update.filter(slot_tuple.in_tuples(keep.iter().copied()).not());
    }
    Ok(update.exec(db).await?.rows_affected as usize)
}

/// The instrument-range arm of the same tick: a value outside what the instrument that measured it
/// can read opens an episode of its own, attributed to that instrument, beside whatever the site
/// and parameter thresholds say. An instrument with no declared range raises nothing.
async fn reconcile_instrument_range<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    slots: Option<&[(Uuid, Uuid)]>,
    spot: bool,
) -> AppResult<SweepStats> {
    let cadence = super::service::cadence_label(spot);
    let breaches = super::service::fetch_instrument_range_rows(
        db,
        &crate::common::authz::AccessScope::Unrestricted,
        slots,
        spot,
    )
    .await?;

    let mut stats = SweepStats::default();
    let txn = db.begin().await?;
    for b in &breaches {
        let sent = alarm_event::ActiveModel {
            id: Set(Uuid::new_v4()),
            site_id: Set(b.site_id),
            parameter_id: Set(b.parameter_id),
            measurement_type: Set(cadence.to_string()),
            kind: Set(KIND_INSTRUMENT_RANGE.to_string()),
            sensor_id: Set(Some(b.sensor_id)),
            severity: Set(super::service::INSTRUMENT_RANGE_SEVERITY),
            max_severity: Set(super::service::INSTRUMENT_RANGE_SEVERITY),
            started_at: Set(b.time.with_timezone(&Utc)),
            value_at_start: Set(b.current_value),
            last_seen_at: Set(b.time.with_timezone(&Utc)),
            last_value: Set(b.current_value),
            ..Default::default()
        };
        let (_episode, status) = upsert::<AlarmEvent, _>(&txn, sent).await?;
        match status {
            UpsertStatus::Created => stats.opened += 1,
            UpsertStatus::Updated | UpsertStatus::Unchanged => stats.updated += 1,
        }
    }
    txn.commit().await?;

    let keep: Vec<(Uuid, Uuid)> = breaches
        .iter()
        .map(|b| (b.site_id, b.parameter_id))
        .collect();
    stats.resolved = resolve_absent(db, slots, &keep, cadence, KIND_INSTRUMENT_RANGE, spot).await?;

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
    alarm_event::Entity::delete_many()
        .filter(alarm_event::Column::SiteId.eq(site_id))
        .filter(alarm_event::Column::ParameterId.eq(parameter_id))
        .filter(alarm_event::Column::ResolvedAt.is_not_null())
        .filter(alarm_event::Column::StartedAt.gte(start))
        .filter(alarm_event::Column::StartedAt.lte(end))
        .exec(db)
        .await?;

    let rows: Vec<alarm_event::ActiveModel> = all_episodes
        .iter()
        .flat_map(|(cadence, episodes)| {
            episodes.iter().map(|ep| alarm_event::ActiveModel {
                id: Set(Uuid::new_v4()),
                site_id: Set(site_id),
                parameter_id: Set(parameter_id),
                measurement_type: Set((*cadence).to_string()),
                severity: Set(ep.severity),
                max_severity: Set(ep.max_severity),
                started_at: Set(ep.started_at.with_timezone(&Utc)),
                value_at_start: Set(ep.value_at_start),
                last_seen_at: Set(ep.last_seen_at.with_timezone(&Utc)),
                last_value: Set(ep.last_value),
                resolved_at: Set(ep.resolved_at.map(|t| t.with_timezone(&Utc))),
                resolved_value: Set(ep.resolved_value),
                ..Default::default()
            })
        })
        .collect();
    let written = rows.len() as i64;
    if !rows.is_empty() {
        alarm_event::Entity::insert_many(rows).exec(db).await?;
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
    let mut query = site_parameters::Entity::find()
        .select_only()
        .column(site_parameters::Column::SiteId)
        .column(site_parameters::Column::ParameterId)
        .distinct()
        .filter(site_parameters::Column::IsActive.eq(true));
    if let Some(s) = site_id {
        query = query.filter(site_parameters::Column::SiteId.eq(s));
    }
    if let Some(p) = parameter_id {
        query = query.filter(site_parameters::Column::ParameterId.eq(p));
    }
    let slots: Vec<(Uuid, Uuid)> = query
        .into_model::<SlotRow>()
        .all(db)
        .await?
        .into_iter()
        // A reading with no slot has no thresholds to evaluate against.
        .filter_map(|r| Some((r.site_id?, r.parameter_id?)))
        .collect();

    let mut total = 0i64;
    for (s, p) in slots {
        let (slot_start, slot_end) = if let (Some(a), Some(b)) = (start, end) {
            (a, b)
        } else {
            // MIN/MAX over an empty slot are NULL, which is the "no readings" case below.
            let extent = readings::Entity::find()
                .select_only()
                .column_as(readings::Column::Time.min(), "lo")
                .column_as(readings::Column::Time.max(), "hi")
                .filter(readings::Column::SiteId.eq(s))
                .filter(readings::Column::ParameterId.eq(p))
                .into_model::<ExtentRow>()
                .one(db)
                .await?;
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
            for (walked, (site_id, parameter_id)) in slots.iter().enumerate() {
                ctx.set_step(walked + 1, slots.len()).await;
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
