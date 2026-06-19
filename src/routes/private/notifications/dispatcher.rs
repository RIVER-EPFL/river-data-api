//! Background notification dispatcher.
//!
//! Wakes on `AlarmStateChanged` broadcasts and on a fallback poll interval, then drains the
//! `alarm_events` outbox: newly-opened events with `notified_at IS NULL`, and newly-resolved events
//! with `resolution_notified_at IS NULL`. Each kind is batched into one message per cycle and handed
//! to every enabled channel. A muted slot is stamped without sending; a slot whose delivery fails on
//! every channel is left unstamped so the next tick retries it.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, Statement};
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;

use crate::common::{AppEvent, EventSender};
use crate::config::Config;

use super::email::{self, EmailChannel};
use super::messages::{self, PendingEvent};
use super::telegram::{TelegramChannel, TelegramClient};
use super::{NotificationChannel, OutgoingMessage, Slot};

struct Row {
    id: Uuid,
    slot: (Uuid, Uuid),
    project_id: Uuid,
    event: PendingEvent,
}

/// Build the enabled channels from config. Empty when nothing is configured (the API runs fine
/// without notifications — the dispatcher then just stamps the outbox and sends nothing).
pub(super) fn build_channels(config: &Config) -> Vec<Box<dyn NotificationChannel>> {
    let mut channels: Vec<Box<dyn NotificationChannel>> = Vec::new();
    if let Some(token) = &config.telegram_bot_token {
        channels.push(Box::new(TelegramChannel::new(TelegramClient::new(token.clone()))));
        tracing::info!("Notifications: Telegram channel enabled");
    }
    match (email::build_mailer(config), config.alert_email_to.clone()) {
        (Some(mailer), Some(to)) => {
            channels.push(Box::new(EmailChannel::new(mailer, to)));
            tracing::info!("Notifications: email channel enabled");
        }
        (Some(_), None) => {
            tracing::warn!("Email backend configured but ALERT_EMAIL_TO unset — email disabled");
        }
        _ => {}
    }
    channels
}

pub async fn periodic(db: DatabaseConnection, config: Arc<Config>, events: EventSender) {
    let channels = build_channels(&config);
    tracing::info!(
        channels = channels.len(),
        poll_secs = config.notify_poll_interval_seconds,
        "Notification dispatcher: starting"
    );

    let mut rx = events.subscribe();
    dispatch_once(&db, &channels, &config).await;

    let mut ticker = tokio::time::interval(Duration::from_secs(config.notify_poll_interval_seconds));
    ticker.tick().await;
    loop {
        let should_run = tokio::select! {
            _ = ticker.tick() => true,
            ev = rx.recv() => match ev {
                Ok(AppEvent::AlarmStateChanged { .. }) => true,
                Ok(_) => false,
                Err(RecvError::Lagged(_)) => true,
                Err(RecvError::Closed) => return,
            },
        };
        if should_run {
            dispatch_once(&db, &channels, &config).await;
        }
    }
}

/// One drain of the outbox (open + resolve passes). Exposed `pub` so integration tests can drive it
/// deterministically with injected channels instead of waiting on the interval.
pub async fn dispatch_once(
    db: &DatabaseConnection,
    channels: &[Box<dyn NotificationChannel>],
    config: &Config,
) {
    if let Err(e) = process_pending(db, channels, config, true).await {
        tracing::warn!(error = %e, "notification dispatcher: open pass failed");
    }
    if let Err(e) = process_pending(db, channels, config, false).await {
        tracing::warn!(error = %e, "notification dispatcher: resolve pass failed");
    }
    // Signal triggers only run when a channel is configured (they do heavier detection queries).
    if !channels.is_empty() {
        super::triggers::run(db, channels, config).await;
    }
}

async fn process_pending(
    db: &DatabaseConnection,
    channels: &[Box<dyn NotificationChannel>],
    config: &Config,
    opened: bool,
) -> Result<(), DbErr> {
    let rows = fetch_pending(db, opened).await?;
    if rows.is_empty() {
        return Ok(());
    }

    let muted_slots = fetch_active_mutes(db).await?;
    let (to_notify, muted): (Vec<Row>, Vec<Row>) =
        rows.into_iter().partition(|r| !muted_slots.contains(&r.slot));

    let column = if opened { "notified_at" } else { "resolution_notified_at" };

    // Suppressed by a mute: stamp so neither this replica nor a peer sends or re-picks it.
    let muted_ids: Vec<Uuid> = muted.iter().map(|r| r.id).collect();
    stamp(db, column, &muted_ids).await?;

    // One message per event so each carries its own slot — fan-out is per-subscriber by scope, which
    // a single batched message spanning multiple slots couldn't express. Each event is CLAIMED with an
    // atomic stamp before the send, so at 2-3 replicas exactly one replica owns it; a transient send
    // failure releases the claim so the next tick retries (at-least-once). The claim is one autocommit
    // UPDATE — no DB connection is held across the external send.
    let base = config.dashboard_base_url.as_deref();
    for r in &to_notify {
        if !claim_event(db, column, r.id).await? {
            continue; // a peer replica already claimed this event
        }
        let events = [r.event.clone()];
        let mut msg = if opened {
            messages::render_opened(&events, base)
        } else {
            messages::render_resolved(&events, base)
        };
        msg.slot = Some(Slot {
            project_id: Some(r.project_id),
            site_id: r.slot.0,
            parameter_id: r.slot.1,
        });
        if !deliver(db, channels, &msg, Some(r.id)).await {
            release_claim(db, column, r.id).await?;
        }
    }
    Ok(())
}

async fn fetch_pending(db: &DatabaseConnection, opened: bool) -> Result<Vec<Row>, DbErr> {
    let sql = if opened {
        "SELECT ae.id, ae.site_id, ae.parameter_id, s.project_id AS project_id, s.name AS site_name, \
                p.name AS parameter_name, p.default_units AS units, \
                ae.severity AS severity, ae.last_value AS value \
         FROM alarm_events ae \
         JOIN sites s ON s.id = ae.site_id \
         JOIN parameters p ON p.id = ae.parameter_id \
         WHERE ae.notified_at IS NULL AND ae.resolved_at IS NULL \
         ORDER BY ae.started_at"
    } else {
        "SELECT ae.id, ae.site_id, ae.parameter_id, s.project_id AS project_id, s.name AS site_name, \
                p.name AS parameter_name, p.default_units AS units, \
                ae.max_severity AS severity, COALESCE(ae.resolved_value, ae.last_value) AS value \
         FROM alarm_events ae \
         JOIN sites s ON s.id = ae.site_id \
         JOIN parameters p ON p.id = ae.parameter_id \
         WHERE ae.resolved_at IS NOT NULL AND ae.resolution_notified_at IS NULL \
         ORDER BY ae.resolved_at"
    };
    let rows = db
        .query_all(Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql.to_string()))
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: Uuid = row.try_get("", "id")?;
        let site_id: Uuid = row.try_get("", "site_id")?;
        let parameter_id: Uuid = row.try_get("", "parameter_id")?;
        out.push(Row {
            id,
            slot: (site_id, parameter_id),
            project_id: row.try_get("", "project_id")?,
            event: PendingEvent {
                site_name: row.try_get("", "site_name")?,
                parameter_name: row.try_get("", "parameter_name")?,
                units: row.try_get("", "units").ok(),
                severity: row.try_get("", "severity")?,
                value: row.try_get("", "value")?,
            },
        });
    }
    Ok(out)
}

async fn fetch_active_mutes(db: &DatabaseConnection) -> Result<HashSet<(Uuid, Uuid)>, DbErr> {
    let rows = db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT site_id, parameter_id FROM notification_mutes \
             WHERE expires_at IS NULL OR expires_at > NOW()"
                .to_string(),
        ))
        .await?;
    let mut set = HashSet::new();
    for row in rows {
        let site_id: Uuid = row.try_get("", "site_id")?;
        let parameter_id: Uuid = row.try_get("", "parameter_id")?;
        set.insert((site_id, parameter_id));
    }
    Ok(set)
}

/// Deliver to every channel, log each attempt, and decide whether to stamp the outbox: stamp when
/// nothing was attempted (no channels/recipients) or at least one delivery succeeded; otherwise leave
/// it for the next tick to retry.
pub(super) async fn deliver(
    db: &DatabaseConnection,
    channels: &[Box<dyn NotificationChannel>],
    msg: &OutgoingMessage,
    single_event_id: Option<Uuid>,
) -> bool {
    let mut attempted = 0usize;
    let mut any_success = false;
    for ch in channels {
        for r in ch.deliver(db, msg).await {
            attempted += 1;
            let (status, error) = match &r.outcome {
                Ok(()) => {
                    any_success = true;
                    ("sent", None)
                }
                Err(e) => ("failed", Some(e.as_str())),
            };
            log_delivery(db, single_event_id, msg.kind, ch.name(), &r.recipient, status, error).await;
        }
    }
    attempted == 0 || any_success
}

pub(super) async fn log_delivery(
    db: &DatabaseConnection,
    alarm_event_id: Option<Uuid>,
    kind: &str,
    channel: &str,
    recipient: &str,
    status: &str,
    error: Option<&str>,
) {
    let res = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO notification_log (alarm_event_id, kind, channel, recipient, status, error) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            [
                alarm_event_id.into(),
                kind.into(),
                channel.into(),
                recipient.into(),
                status.into(),
                error.into(),
            ],
        ))
        .await;
    if let Err(e) = res {
        tracing::warn!(error = %e, "failed to write notification_log row");
    }
}

async fn stamp(db: &DatabaseConnection, column: &str, ids: &[Uuid]) -> Result<(), DbErr> {
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("${i}")).collect();
    let sql = format!(
        "UPDATE alarm_events SET {column} = NOW(), updated_at = NOW() WHERE id IN ({})",
        placeholders.join(",")
    );
    let values: Vec<sea_orm::Value> = ids.iter().map(|id| (*id).into()).collect();
    db.execute(Statement::from_sql_and_values(sea_orm::DatabaseBackend::Postgres, &sql, values))
        .await?;
    Ok(())
}

/// Atomically claim one outbox event by stamping its sent-marker column iff still NULL. The single
/// replica whose UPDATE flips it from NULL wins (gets a RETURNING row) and sends; a peer that lost the
/// race gets no row and skips. `column` is an internal literal, never user input.
async fn claim_event(db: &DatabaseConnection, column: &str, id: Uuid) -> Result<bool, DbErr> {
    let sql = format!(
        "UPDATE alarm_events SET {column} = NOW(), updated_at = NOW() \
         WHERE id = $1 AND {column} IS NULL RETURNING id"
    );
    let row = db
        .query_one(Statement::from_sql_and_values(sea_orm::DatabaseBackend::Postgres, &sql, [id.into()]))
        .await?;
    Ok(row.is_some())
}

/// Release a claim after an all-channel send failure so the next tick retries it (at-least-once).
async fn release_claim(db: &DatabaseConnection, column: &str, id: Uuid) -> Result<(), DbErr> {
    let sql = format!("UPDATE alarm_events SET {column} = NULL WHERE id = $1");
    db.execute(Statement::from_sql_and_values(sea_orm::DatabaseBackend::Postgres, &sql, [id.into()]))
        .await?;
    Ok(())
}
