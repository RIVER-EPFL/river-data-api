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
use std::time::Duration;

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
    let breaches = fetch_active_alarm_rows(db, None).await?;

    // Pairs that already have an open event (so we can count opened vs updated).
    let mut open_keys: HashSet<(Uuid, Uuid)> = HashSet::new();
    for row in db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT site_id, parameter_id FROM alarm_events WHERE resolved_at IS NULL".to_string(),
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
                (site_id, parameter_id, severity, max_severity, started_at, value_at_start, last_seen_at, last_value) \
             VALUES ($1, $2, $3, $3, $4, $5, $4, $5) \
             ON CONFLICT (site_id, parameter_id) WHERE resolved_at IS NULL \
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
    // resolving value. Empty breach set → resolve everything still open.
    let keep: Vec<(Uuid, Uuid)> = breaches.iter().map(|b| (b.site_id, b.parameter_id)).collect();
    let mut values: Vec<sea_orm::Value> = Vec::new();
    let not_in_clause = if keep.is_empty() {
        String::new()
    } else {
        let mut pairs = Vec::with_capacity(keep.len());
        for (i, (s, p)) in keep.iter().enumerate() {
            pairs.push(format!("(${},${})", i * 2 + 1, i * 2 + 2));
            values.push((*s).into());
            values.push((*p).into());
        }
        format!(" AND (ae.site_id, ae.parameter_id) NOT IN ({})", pairs.join(","))
    };
    let resolve_sql = format!(
        "UPDATE alarm_events ae \
         SET resolved_at = NOW(), \
             updated_at = NOW(), \
             resolved_value = (SELECT COALESCE(r.calibrated_value, r.raw_value) FROM readings r \
                               WHERE r.site_id = ae.site_id AND r.parameter_id = ae.parameter_id \
                               ORDER BY r.time DESC LIMIT 1) \
         WHERE ae.resolved_at IS NULL{not_in_clause}"
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

/// Long-running task: sweep once on startup, then every `interval`. Emits an `AlarmStateChanged` SSE
/// event whenever a tick opens or resolves something, so the dashboard can refresh reactively.
pub async fn periodic(db: DatabaseConnection, interval: Duration, events: EventSender) {
    tracing::info!(interval_secs = interval.as_secs(), "Alarm sweeper: starting");

    let tick = async |db: &DatabaseConnection, events: &EventSender| match evaluate_alarm_events(db).await {
        Ok(stats) => {
            if stats.opened > 0 || stats.resolved > 0 {
                tracing::info!(
                    opened = stats.opened,
                    updated = stats.updated,
                    resolved = stats.resolved,
                    "Alarm sweeper: state changed"
                );
                let _ = events.send(AppEvent::AlarmStateChanged {
                    opened: stats.opened,
                    resolved: stats.resolved,
                });
            }
        }
        Err(e) => tracing::warn!(error = %e, "Alarm sweeper: tick failed"),
    };

    tick(&db, &events).await;

    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        tick(&db, &events).await;
    }
}
