//! Background alarm sweeper: reconciles persisted `alarm_events` against the current breach set.
//!
//! Reuses the exact breach query that backs `/alarms/active` (`fetch_active_alarm_rows`) so persisted
//! events never diverge from what the live feed computes. Each tick opens an event for any new breach,
//! refreshes severity/last-seen on still-breaching events, and stamps `resolved_at` on open events
//! whose reading has returned to range (auto-resolve). A resolved event sits outside the partial
//! unique index, so a fresh breach of the same pair opens a brand-new event (re-raise) and history is
//! preserved.
//!
//! `evaluate_alarm_events` is the single-tick logic, exposed `pub` so integration tests can drive it
//! deterministically without waiting on the interval (the sweeper task is only spawned in `main.rs`,
//! never under `build_test_app`).

use std::collections::HashSet;

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use crate::common::{AppEvent, EventSender};
use crate::error::AppResult;

use super::views::fetch_active_alarm_rows;

#[derive(Debug, Default, Clone, Copy)]
pub struct SweepStats {
    pub opened: usize,
    pub updated: usize,
    pub resolved: usize,
}

/// One reconciliation tick. Idempotent: safe to call repeatedly (the partial unique index on open
/// events makes open-or-update a no-op when nothing changed).
pub async fn evaluate_alarm_events(db: &DatabaseConnection) -> AppResult<SweepStats> {
    reconcile(db, None).await
}

/// Scoped reconcile for just the given `(site_id, parameter_id)` slots, the event-driven entry
/// point (ingest / threshold / config change). Only opens/updates/resolves events within these
/// slots; alarms outside them are never touched (so it's safe to fire on a partial change).
pub async fn reconcile_open_alarms(
    db: &DatabaseConnection,
    slots: &[(Uuid, Uuid)],
) -> AppResult<SweepStats> {
    reconcile(db, Some(slots)).await
}

/// Event-driven call sites use this: reconcile the given slots and emit an `AlarmStateChanged` SSE
/// if anything opened or resolved (mirroring the periodic tick). Never fails the caller, it logs
/// and swallows errors, so wiring it into a write/config path can never break that path. The
/// periodic backstop still reconciles everything regardless.
pub async fn reconcile_and_notify(
    db: &DatabaseConnection,
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
pub async fn reconcile_all_and_notify(db: &DatabaseConnection, events: &EventSender) {
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

/// [`reconcile_all_and_notify`] for contexts that only hold a `&DatabaseConnection` (CrudCrate
/// operation hooks). Uses the process-global event sender; a missing sender (some unit tests) just
/// skips the SSE. Never returns an error, a failed reconcile must not fail the CRUD operation
/// that triggered it.
pub async fn reconcile_all_from_hook(db: &DatabaseConnection) {
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
async fn reconcile(
    db: &DatabaseConnection,
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

async fn reconcile_cadence(
    db: &DatabaseConnection,
    slots: Option<&[(Uuid, Uuid)]>,
    spot: bool,
) -> AppResult<SweepStats> {
    let cadence = super::views::cadence_label(spot);
    let cadence_pred = super::views::cadence_predicate(spot);

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
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &open_keys_sql,
            open_keys_values,
        ))
        .await?
    {
        let site_id: Uuid = row.try_get("", "site_id")?;
        let parameter_id: Uuid = row.try_get("", "parameter_id")?;
        open_keys.insert((site_id, parameter_id));
    }

    let mut stats = SweepStats::default();

    // Open-or-update each current breach.
    for b in &breaches {
        let is_new = !open_keys.contains(&(b.site_id, b.parameter_id));
        db.execute(Statement::from_sql_and_values(
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
    let resolve_sql = format!(
        "UPDATE alarm_events ae \
         SET resolved_at = NOW(), \
             updated_at = NOW(), \
             resolved_value = (SELECT COALESCE(smp.mean, r.calibrated_value, r.raw_value) FROM readings r \
                               LEFT JOIN samples smp ON smp.id = r.sample_id \
                               WHERE r.site_id = ae.site_id AND r.parameter_id = ae.parameter_id \
                                 AND {cadence_pred} \
                               ORDER BY r.time DESC, (r.is_flagged IS TRUE), \
                                        r.replicate_index LIMIT 1) \
         WHERE ae.resolved_at IS NULL AND ae.measurement_type = '{cadence}'{scope_clause}{not_in_clause}"
    );
    let resolved = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &resolve_sql,
            values,
        ))
        .await?
        .rows_affected() as usize;
    stats.resolved = resolved;

    Ok(stats)
}
