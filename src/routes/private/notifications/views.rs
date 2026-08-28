//! Admin HTTP handlers for the notification layer.

use axum::{Json, extract::State};
use sea_orm::{ConnectionTrait, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

use super::NotificationChannel;
use super::dispatcher::log_delivery;

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;

#[derive(Debug, Deserialize, ToSchema)]
pub struct TestSendRequest {
    pub channel: String,
    pub recipient: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TestResult {
    pub recipient: String,
    pub status: String,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestSendResponse {
    pub channel: String,
    pub results: Vec<TestResult>,
    pub all_sent: bool,
}

#[utoipa::path(
    post,
    path = "/notifications/test-send",
    request_body = TestSendRequest,
    responses((status = 200, description = "Test attempted", body = TestSendResponse)),
    tag = "notifications"
)]
pub async fn test_send(
    State(state): State<AppState>,
    Json(req): Json<TestSendRequest>,
) -> AppResult<Json<TestSendResponse>> {
    let recipient = req.recipient.trim().to_string();
    if recipient.is_empty() {
        return Err(AppError::BadRequest("recipient is required".to_string()));
    }

    let outcome = match req.channel.as_str() {
        "web_push" => {
            if !state.config.web_push_configured() {
                return Err(AppError::BadRequest(
                    "Web Push is not configured".to_string(),
                ));
            }
            let channel = super::web_push::WebPushChannel::new(&state.config)
                .ok_or_else(|| AppError::Internal("failed to build web push channel".to_string()))?;
            let msg = super::OutgoingMessage {
                kind: "test",
                subject: "River Data test notification".to_string(),
                body: "✅ This is a test notification from River Data.".to_string(),
                slot: None,
            };
            let results = channel.deliver(&state, &msg).await;
            if results.is_empty() {
                Err("no subscriptions found for this user".to_string())
            } else if let Some(err) = results.iter().find_map(|r| r.outcome.as_ref().err()) {
                Err(err.clone())
            } else {
                Ok(())
            }
        }
        other => {
            return Err(AppError::BadRequest(format!("unknown channel: {other}")));
        }
    };

    let (status, error) = match &outcome {
        Ok(()) => ("sent", None),
        Err(e) => ("failed", Some(e.as_str())),
    };
    log_delivery(
        &state.db,
        None,
        "test",
        &req.channel,
        &recipient,
        status,
        error,
    )
    .await;

    Ok(Json(TestSendResponse {
        channel: req.channel,
        all_sent: outcome.is_ok(),
        results: vec![TestResult {
            recipient,
            status: status.to_string(),
            error: error.map(str::to_string),
        }],
    }))
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubscriberRow {
    pub keycloak_sub: String,
    pub web_push_enabled: bool,
    pub is_active: bool,
    pub push_subscription_count: i64,
    pub subscription_overrides: i64,
}

#[utoipa::path(
    get,
    path = "/notifications/subscribers",
    responses((status = 200, description = "Subscriber roster", body = [SubscriberRow])),
    tag = "notifications"
)]
pub async fn list_subscribers(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<SubscriberRow>>> {
    let rows = state
        .db
        .query_all(Statement::from_string(
            PG,
            "WITH subs AS ( \
                SELECT keycloak_sub FROM notification_subscribers \
                UNION \
                SELECT DISTINCT keycloak_sub FROM web_push_subscriptions \
             ) \
             SELECT s.keycloak_sub, \
                COALESCE(ns.is_active, true) AS is_active, \
                COALESCE(ns.web_push_enabled, true) AS web_push_enabled, \
                (SELECT COUNT(*) FROM notification_subscriptions nsub \
                   WHERE nsub.keycloak_sub = s.keycloak_sub) AS overrides, \
                (SELECT COUNT(*) FROM web_push_subscriptions wps \
                   WHERE wps.keycloak_sub = s.keycloak_sub) AS push_count \
             FROM subs s \
             LEFT JOIN notification_subscribers ns ON ns.keycloak_sub = s.keycloak_sub \
             ORDER BY s.keycloak_sub"
                .to_string(),
        ))
        .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(SubscriberRow {
            keycloak_sub: r.try_get("", "keycloak_sub")?,
            web_push_enabled: r.try_get("", "web_push_enabled").unwrap_or(true),
            is_active: r.try_get("", "is_active")?,
            push_subscription_count: r.try_get("", "push_count")?,
            subscription_overrides: r.try_get("", "overrides")?,
        });
    }
    Ok(Json(out))
}
