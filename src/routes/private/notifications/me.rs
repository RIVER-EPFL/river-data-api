//! Self-service notification preferences. Every handler acts strictly on the caller's own Keycloak
//! `sub` (taken from the JWT, never from the request body), so a user can only ever read or change
//! their own subscription state and push subscriptions.

use axum::{Extension, Json, extract::State, http::StatusCode};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, Statement, TransactionTrait};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use std::collections::HashSet;

use crate::common::AppState;
use crate::common::authz::AccessScope;
use crate::common::middleware::AuthContext;
use crate::error::{AppError, AppResult};

use super::access::project_allowed;
use super::dispatcher::log_delivery;

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;

fn require_sub(auth: &AuthContext) -> AppResult<String> {
    auth.keycloak_sub().map(str::to_string).ok_or_else(|| {
        AppError::Forbidden("notification preferences require a Keycloak login".to_string())
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SubscriptionScope {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub project_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub site_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parameter_id: Option<Uuid>,
    pub enabled: bool,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MyNotifications {
    pub web_push_enabled: bool,
    pub push_subscription_count: i64,
    pub subscriptions: Vec<SubscriptionScope>,
}

async fn ensure_subscriber(state: &AppState, sub: &str) -> AppResult<()> {
    state
        .db
        .execute(Statement::from_sql_and_values(
            PG,
            "INSERT INTO notification_subscribers (keycloak_sub) VALUES ($1) \
             ON CONFLICT (keycloak_sub) DO NOTHING",
            [sub.into()],
        ))
        .await?;
    Ok(())
}

async fn load(state: &AppState, sub: &str) -> AppResult<MyNotifications> {
    let row = state
        .db
        .query_one(Statement::from_sql_and_values(
            PG,
            "SELECT COALESCE(ns.web_push_enabled, true) AS web_push_enabled \
             FROM notification_subscribers ns WHERE ns.keycloak_sub = $1",
            [sub.into()],
        ))
        .await?;
    let web_push_enabled = row
        .as_ref()
        .and_then(|r| r.try_get::<bool>("", "web_push_enabled").ok())
        .unwrap_or(true);

    let push_count = state
        .db
        .query_one(Statement::from_sql_and_values(
            PG,
            "SELECT COUNT(*) AS cnt FROM web_push_subscriptions WHERE keycloak_sub = $1",
            [sub.into()],
        ))
        .await?
        .and_then(|r| r.try_get::<i64>("", "cnt").ok())
        .unwrap_or(0);

    let sub_rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            PG,
            "SELECT project_id, site_id, parameter_id, enabled FROM notification_subscriptions \
             WHERE keycloak_sub = $1",
            [sub.into()],
        ))
        .await?;
    let subscriptions = sub_rows
        .iter()
        .map(|r| SubscriptionScope {
            project_id: r.try_get("", "project_id").ok().flatten(),
            site_id: r.try_get("", "site_id").ok().flatten(),
            parameter_id: r.try_get("", "parameter_id").ok().flatten(),
            enabled: r.try_get("", "enabled").unwrap_or(true),
        })
        .collect();

    Ok(MyNotifications {
        web_push_enabled,
        push_subscription_count: push_count,
        subscriptions,
    })
}

#[utoipa::path(
    get,
    path = "/notifications/me",
    responses((status = 200, description = "My notification settings", body = MyNotifications)),
    tag = "notifications"
)]
pub async fn get_my_notifications(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> AppResult<Json<MyNotifications>> {
    let sub = require_sub(&auth)?;
    ensure_subscriber(&state, &sub).await?;
    Ok(Json(load(&state, &sub).await?))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdatePrefsRequest {
    pub web_push_enabled: Option<bool>,
}

#[utoipa::path(
    patch,
    path = "/notifications/me",
    request_body = UpdatePrefsRequest,
    responses((status = 200, description = "Updated settings", body = MyNotifications)),
    tag = "notifications"
)]
pub async fn update_my_notifications(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<UpdatePrefsRequest>,
) -> AppResult<Json<MyNotifications>> {
    let sub = require_sub(&auth)?;
    ensure_subscriber(&state, &sub).await?;
    state
        .db
        .execute(Statement::from_sql_and_values(
            PG,
            "UPDATE notification_subscribers \
             SET web_push_enabled = COALESCE($2, web_push_enabled), \
                 updated_at = NOW() \
             WHERE keycloak_sub = $1",
            [sub.clone().into(), req.web_push_enabled.into()],
        ))
        .await?;
    Ok(Json(load(&state, &sub).await?))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SetSubscriptionsRequest {
    pub subscriptions: Vec<SubscriptionScope>,
}

#[utoipa::path(
    put,
    path = "/notifications/me/subscriptions",
    request_body = SetSubscriptionsRequest,
    responses((status = 200, description = "Updated settings", body = MyNotifications)),
    tag = "notifications"
)]
pub async fn set_my_subscriptions(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<SetSubscriptionsRequest>,
) -> AppResult<Json<MyNotifications>> {
    let sub = require_sub(&auth)?;

    let scope = auth.access_scope();
    let accessible: Option<HashSet<Uuid>> = match &scope {
        AccessScope::Unrestricted => None,
        AccessScope::Projects(projects) => Some((**projects).clone()),
    };

    for s in &req.subscriptions {
        if let Some(pid) = s.project_id
            && !project_allowed(&accessible, pid)
        {
            return Err(AppError::Forbidden(format!(
                "project {pid} is not in your grant set"
            )));
        }
    }

    let txn = state.db.begin().await?;
    txn.execute(Statement::from_sql_and_values(
        PG,
        "DELETE FROM notification_subscriptions WHERE keycloak_sub = $1",
        [sub.clone().into()],
    ))
    .await?;
    for s in &req.subscriptions {
        txn.execute(Statement::from_sql_and_values(
            PG,
            "INSERT INTO notification_subscriptions \
                (keycloak_sub, project_id, site_id, parameter_id, enabled) \
             VALUES ($1, $2, $3, $4, $5)",
            [
                sub.clone().into(),
                s.project_id.into(),
                s.site_id.into(),
                s.parameter_id.into(),
                s.enabled.into(),
            ],
        ))
        .await?;
    }
    txn.commit().await?;

    Ok(Json(load(&state, &sub).await?))
}

// ---------------------------------------------------------------------------
// Web Push subscription CRUD
// ---------------------------------------------------------------------------

#[derive(Deserialize, ToSchema)]
pub struct RegisterPushRequest {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub user_agent: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct PushSubscriptionRow {
    pub id: Uuid,
    pub endpoint: String,
    pub user_agent: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_success_at: Option<DateTime<Utc>>,
}

#[utoipa::path(post, path = "/api/notifications/me/push", tag = "notifications")]
pub async fn register_push_subscription(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<RegisterPushRequest>,
) -> AppResult<Json<PushSubscriptionRow>> {
    let sub = require_sub(&auth)?;
    let row = state
        .db
        .query_one(Statement::from_sql_and_values(
            PG,
            "INSERT INTO web_push_subscriptions (keycloak_sub, endpoint, p256dh, auth, user_agent) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (endpoint) DO UPDATE SET \
                 keycloak_sub = EXCLUDED.keycloak_sub, \
                 p256dh = EXCLUDED.p256dh, \
                 auth = EXCLUDED.auth, \
                 user_agent = EXCLUDED.user_agent \
             RETURNING id, endpoint, user_agent, created_at, last_success_at",
            [
                sub.into(),
                body.endpoint.into(),
                body.p256dh.into(),
                body.auth.into(),
                body.user_agent.into(),
            ],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound("subscription".to_string()))?;

    Ok(Json(PushSubscriptionRow {
        id: row.try_get("", "id")?,
        endpoint: row.try_get("", "endpoint")?,
        user_agent: row.try_get("", "user_agent")?,
        created_at: row.try_get("", "created_at")?,
        last_success_at: row.try_get("", "last_success_at")?,
    }))
}

#[utoipa::path(get, path = "/api/notifications/me/push", tag = "notifications")]
pub async fn list_push_subscriptions(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> AppResult<Json<Vec<PushSubscriptionRow>>> {
    let sub = require_sub(&auth)?;
    let rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            PG,
            "SELECT id, endpoint, user_agent, created_at, last_success_at \
             FROM web_push_subscriptions WHERE keycloak_sub = $1 ORDER BY created_at DESC",
            [sub.into()],
        ))
        .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(PushSubscriptionRow {
            id: row.try_get("", "id")?,
            endpoint: row.try_get("", "endpoint")?,
            user_agent: row.try_get("", "user_agent")?,
            created_at: row.try_get("", "created_at")?,
            last_success_at: row.try_get("", "last_success_at")?,
        });
    }
    Ok(Json(out))
}

#[derive(Deserialize, ToSchema)]
pub struct DeletePushRequest {
    pub endpoint: String,
}

#[utoipa::path(delete, path = "/api/notifications/me/push", tag = "notifications")]
pub async fn delete_push_subscription(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<DeletePushRequest>,
) -> AppResult<StatusCode> {
    let sub = require_sub(&auth)?;
    state
        .db
        .execute(Statement::from_sql_and_values(
            PG,
            "DELETE FROM web_push_subscriptions WHERE keycloak_sub = $1 AND endpoint = $2",
            [sub.into(), body.endpoint.into()],
        ))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Self-service test + timed ping
// ---------------------------------------------------------------------------

#[derive(Deserialize, ToSchema)]
pub struct PingRequest {
    #[serde(default = "default_ping_seconds")]
    pub seconds: u64,
}

fn default_ping_seconds() -> u64 {
    10
}

#[utoipa::path(post, path = "/api/notifications/me/push/test", tag = "notifications")]
pub async fn test_push(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> AppResult<Json<Vec<PushAttempt>>> {
    let sub = require_sub(&auth)?;
    let attempts =
        send_to_user(&state, &sub, "Test notification", "Push notifications are working.").await?;
    Ok(Json(attempts))
}

#[utoipa::path(post, path = "/api/notifications/me/push/ping", tag = "notifications")]
pub async fn schedule_ping(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<PingRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let sub = require_sub(&auth)?;
    let seconds = body.seconds.clamp(5, 3600);
    let owned_sub = sub.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
        let _ = send_to_user(&state, &owned_sub, "Ping", &format!("Your {seconds}-second ping.")).await;
    });
    Ok(Json(serde_json::json!({ "seconds": seconds })))
}

/// One device's outcome from a self-service push. `endpoint_tail` is the last 12 characters of the
/// endpoint: enough to tell two devices apart in a log or in the UI, useless as a capability.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PushAttempt {
    pub id: Uuid,
    pub endpoint_tail: String,
    pub user_agent: Option<String>,
    pub status: String,
    pub error: Option<String>,
    pub pruned: bool,
}

fn endpoint_tail(endpoint: &str) -> String {
    let count = endpoint.chars().count();
    endpoint.chars().skip(count.saturating_sub(12)).collect()
}

async fn send_to_user(
    state: &AppState,
    keycloak_sub: &str,
    title: &str,
    body: &str,
) -> AppResult<Vec<PushAttempt>> {
    let rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            PG,
            "SELECT id, endpoint, p256dh, auth, user_agent \
             FROM web_push_subscriptions WHERE keycloak_sub = $1 ORDER BY created_at",
            [keycloak_sub.into()],
        ))
        .await?;

    if rows.is_empty() {
        return Err(AppError::BadRequest("no push subscriptions registered".to_string()));
    }

    let Some(pem) = &state.config.vapid_private_key_pem else {
        return Err(AppError::Internal("VAPID not configured".to_string()));
    };
    let Some(vapid_subject) = &state.config.vapid_subject else {
        return Err(AppError::Internal("VAPID subject not configured".to_string()));
    };

    // A unique tag per send: notifications sharing a tag replace each other, so a fixed
    // "test" tag makes the second test silently overwrite the first instead of alerting.
    let payload = serde_json::json!({
        "title": title,
        "body": body,
        "tag": format!("test-{}", Utc::now().timestamp_millis()),
    })
    .to_string();

    let client = reqwest::Client::new();
    let mut attempts = Vec::with_capacity(rows.len());

    for row in rows {
        // A row that will not decode must not silence the devices queued behind it.
        let decoded = (
            row.try_get::<Uuid>("", "id"),
            row.try_get::<String>("", "endpoint"),
            row.try_get::<String>("", "p256dh"),
            row.try_get::<String>("", "auth"),
            row.try_get::<Option<String>>("", "user_agent"),
        );
        let (id, endpoint, p256dh, auth_key, user_agent) = match decoded {
            (Ok(id), Ok(endpoint), Ok(p256dh), Ok(auth_key), Ok(user_agent)) => {
                (id, endpoint, p256dh, auth_key, user_agent)
            }
            _ => {
                tracing::warn!("push: skipping undecodable subscription row");
                continue;
            }
        };

        let tail = endpoint_tail(&endpoint);
        let sub = super::web_push::Subscription {
            id,
            keycloak_sub: keycloak_sub.to_string(),
            endpoint,
            p256dh,
            auth: auth_key,
        };

        let attempt = match super::web_push::send_push(
            &client,
            pem.as_bytes(),
            vapid_subject,
            &sub,
            payload.as_bytes(),
        )
        .await
        {
            Ok(()) => {
                super::web_push::stamp_success(&state.db, id).await;
                PushAttempt {
                    id,
                    endpoint_tail: tail.clone(),
                    user_agent,
                    status: "sent".to_string(),
                    error: None,
                    pruned: false,
                }
            }
            Err(e) => {
                // 410 and 404 mean the push service has retired the endpoint: it can never
                // deliver again, so the row goes rather than failing on every future send.
                let gone = e.contains("EndpointNotValid") || e.contains("EndpointNotFound");
                if gone {
                    super::web_push::prune_subscription(&state.db, id).await;
                }
                tracing::warn!(error = %e, endpoint_tail = %tail, pruned = gone, "push: delivery failed");
                PushAttempt {
                    id,
                    endpoint_tail: tail.clone(),
                    user_agent,
                    status: "failed".to_string(),
                    error: Some(e),
                    pruned: gone,
                }
            }
        };

        log_delivery(
            &state.db,
            None,
            "test",
            "web_push",
            &tail,
            &attempt.status,
            attempt.error.as_deref(),
        )
        .await;

        attempts.push(attempt);
    }

    Ok(attempts)
}
