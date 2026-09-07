//! Channel health heartbeat. A background probe checks each configured channel and records the
//! result as a `notification_state` row of kind `channel_health`; the admin endpoint reads the
//! latest persisted state so the dashboard shows reachability and a last-checked time. Web Push
//! is the one channel today.

use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde::Serialize;

use crate::common::AppState;
use crate::config::Config;
use crate::error::AppResult;

use super::dispatcher::build_channels;

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;

/// The `notification_state` kind a channel's health is recorded under, so the probe and the read
/// cannot drift apart.
const CHANNEL_HEALTH_KIND: &str = "channel_health";

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChannelHealth {
    pub name: String,
    pub available: bool,
    /// `None` until a probe has run for this channel.
    pub healthy: Option<bool>,
    pub detail: Option<String>,
    pub checked_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NotificationHealth {
    pub channels: Vec<ChannelHealth>,
    /// No channel resolves from config, so every notification is stamped undeliverable. The API
    /// runs regardless (notifications never block ingestion), which is exactly why this has to be
    /// visible rather than inferred from an empty list.
    pub no_channel_configured: bool,
    /// Deliveries in the last 24 hours that reached nobody: `undeliverable` (no channel at all) and
    /// `failed` (a channel refused the send).
    pub undeliverable_24h: i64,
    pub failed_24h: i64,
}

/// Probe every configured channel and upsert its health row, returning how many were probed. A
/// no-op when nothing is configured.
pub async fn probe_once(db: &DatabaseConnection, config: &Config) -> usize {
    let channels = build_channels(config);
    for ch in &channels {
        let (healthy, detail) = match ch.check_health().await {
            Ok(d) => (true, d),
            Err(e) => (false, e),
        };
        let res = db
            .execute_raw(Statement::from_sql_and_values(
                PG,
                "INSERT INTO notification_state (kind, subject_key, state, last_notified_at, \
                     detail) \
                 VALUES ($1, $2, $3, NOW(), $4) \
                 ON CONFLICT (kind, subject_key) DO UPDATE SET state = EXCLUDED.state, \
                     detail = EXCLUDED.detail, last_notified_at = EXCLUDED.last_notified_at",
                [
                    CHANNEL_HEALTH_KIND.into(),
                    ch.name().into(),
                    if healthy { "healthy" } else { "unhealthy" }.into(),
                    detail.into(),
                ],
            ))
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, channel = ch.name(), "failed to upsert channel health");
        }
    }
    channels.len()
}

async fn read_health(db: &DatabaseConnection, config: &Config) -> NotificationHealth {
    let known = [("web_push", config.web_push_configured())];
    let mut channels = Vec::with_capacity(known.len());
    for (name, available) in known {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                PG,
                "SELECT state = 'healthy' AS healthy, detail, last_notified_at AS checked_at \
                   FROM notification_state \
                  WHERE kind = $1 AND subject_key = $2",
                [CHANNEL_HEALTH_KIND.into(), name.into()],
            ))
            .await
            .ok()
            .flatten();
        // A health report degrades rather than fails: a row it cannot read is a channel whose
        // state is unknown, which is what `None` says on every one of these three.
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
    let (undeliverable_24h, failed_24h) = recent_failures(db).await;
    NotificationHealth {
        no_channel_configured: channels.iter().all(|c| !c.available),
        channels,
        undeliverable_24h,
        failed_24h,
    }
}

/// How many deliveries reached nobody in the last day, by kind of failure.
async fn recent_failures(db: &DatabaseConnection) -> (i64, i64) {
    let row = db
        .query_one_raw(Statement::from_string(
            PG,
            "SELECT \
                 COUNT(*) FILTER (WHERE status = 'undeliverable')::bigint AS undeliverable, \
                 COUNT(*) FILTER (WHERE status = 'failed')::bigint AS failed \
             FROM notification_log WHERE created_at > NOW() - INTERVAL '24 hours'"
                .to_string(),
        ))
        .await;
    // Both columns are cast to bigint in the query and a FILTER count is never NULL, so a default
    // here is unreachable; it stands because this function reports health and must not fail.
    match row {
        Ok(Some(r)) => (
            r.try_get::<i64>("", "undeliverable").unwrap_or(0),
            r.try_get::<i64>("", "failed").unwrap_or(0),
        ),
        Ok(None) => (0, 0),
        Err(e) => {
            tracing::warn!(error = %e, "failed to count recent notification failures");
            (0, 0)
        }
    }
}

/// `GET /api/notifications/health`, latest persisted health per channel (admin-only).
#[utoipa::path(
    get,
    path = "/api/notifications/health",
    responses((status = 200, description = "Latest persisted health per channel", body = NotificationHealth)),
    tag = "notifications"
)]
pub async fn get_health(State(state): State<AppState>) -> AppResult<Json<NotificationHealth>> {
    Ok(Json(read_health(&state.db, &state.config).await))
}

/// `POST /api/notifications/health/refresh`, probe now, then return the fresh state (admin-only).
#[utoipa::path(
    post,
    path = "/api/notifications/health/refresh",
    responses((status = 200, description = "Health after probing every configured channel", body = NotificationHealth)),
    tag = "notifications"
)]
pub async fn refresh_health(State(state): State<AppState>) -> AppResult<Json<NotificationHealth>> {
    let _ = probe_once(&state.db, &state.config).await;
    Ok(Json(read_health(&state.db, &state.config).await))
}
