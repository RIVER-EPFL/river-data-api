//! Channel health heartbeat. A background probe checks each configured channel (Telegram `getMe`,
//! SMTP connection test, or Graph token fetch) and upserts `notification_channel_health`; the admin
//! endpoint reads the latest persisted state so the dashboard shows reachability + a last-checked time.

use std::sync::Arc;
use std::time::Duration;

use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde::Serialize;

use crate::common::AppState;
use crate::config::Config;
use crate::error::AppResult;

use super::dispatcher::build_channels;

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelHealth {
    pub name: String,
    pub available: bool,
    /// `None` until a probe has run for this channel.
    pub healthy: Option<bool>,
    pub detail: Option<String>,
    pub checked_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
pub struct NotificationHealth {
    pub channels: Vec<ChannelHealth>,
}

/// Probe every configured channel and upsert its health row. A no-op when nothing is configured.
pub async fn probe_once(db: &DatabaseConnection, config: &Config) {
    for ch in &build_channels(config) {
        let (healthy, detail) = match ch.check_health().await {
            Ok(d) => (true, d),
            Err(e) => (false, e),
        };
        let res = db
            .execute(Statement::from_sql_and_values(
                PG,
                "INSERT INTO notification_channel_health (channel, healthy, detail, checked_at) \
                 VALUES ($1, $2, $3, NOW()) \
                 ON CONFLICT (channel) DO UPDATE SET healthy = EXCLUDED.healthy, \
                     detail = EXCLUDED.detail, checked_at = EXCLUDED.checked_at",
                [ch.name().into(), healthy.into(), detail.into()],
            ))
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, channel = ch.name(), "failed to upsert channel health");
        }
    }
}

/// Background loop: probe on startup, then every `notify_health_interval_seconds` (min 30s).
pub async fn periodic(db: DatabaseConnection, config: Arc<Config>) {
    let mut ticker =
        tokio::time::interval(Duration::from_secs(config.notify_health_interval_seconds.max(30)));
    loop {
        ticker.tick().await;
        probe_once(&db, &config).await;
    }
}

async fn read_health(db: &DatabaseConnection, config: &Config) -> NotificationHealth {
    let known = [
        ("telegram", config.telegram_configured()),
        ("email", config.email_configured()),
    ];
    let mut channels = Vec::with_capacity(known.len());
    for (name, available) in known {
        let row = db
            .query_one(Statement::from_sql_and_values(
                PG,
                "SELECT healthy, detail, checked_at FROM notification_channel_health \
                 WHERE channel = $1",
                [name.into()],
            ))
            .await
            .ok()
            .flatten();
        let (healthy, detail, checked_at) = match row {
            Some(r) => (
                r.try_get::<bool>("", "healthy").ok(),
                r.try_get::<Option<String>>("", "detail").ok().flatten(),
                r.try_get::<DateTime<Utc>>("", "checked_at").ok(),
            ),
            None => (None, None, None),
        };
        channels.push(ChannelHealth {
            name: name.to_string(),
            available,
            healthy,
            detail,
            checked_at,
        });
    }
    NotificationHealth { channels }
}

/// `GET /api/notifications/health` — latest persisted health per channel (admin-only).
pub async fn get_health(State(state): State<AppState>) -> AppResult<Json<NotificationHealth>> {
    Ok(Json(read_health(&state.db, &state.config).await))
}

/// `POST /api/notifications/health/refresh` — probe now, then return the fresh state (admin-only).
pub async fn refresh_health(State(state): State<AppState>) -> AppResult<Json<NotificationHealth>> {
    probe_once(&state.db, &state.config).await;
    Ok(Json(read_health(&state.db, &state.config).await))
}
