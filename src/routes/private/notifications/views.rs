//! Admin HTTP handlers for the notification layer (link-code minting).

use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use rand::Rng;
use sea_orm::{ConnectionTrait, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

use super::dispatcher::log_delivery;
use super::email::build_mailer;
use super::telegram::TelegramClient;

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;
const LINK_CODE_TTL_MINUTES: i64 = 60;

#[derive(Debug, Deserialize, ToSchema)]
pub struct GenerateLinkCodeRequest {
    /// Keycloak user `sub` this chat will speak for. The bot's role checks resolve against it.
    pub keycloak_sub: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LinkCodeResponse {
    pub code: String,
    pub expires_at: DateTime<Utc>,
}

/// Avoids visually ambiguous characters (no l/1/i/o/0) so codes are easy to relay.
pub(super) fn generate_code() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::rng();
    (0..8)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

/// Mint a one-time link code for a Keycloak user. The user sends `/start <code>` to the bot to
/// claim it. Any prior unclaimed code for the same user is dropped, so a user has at most one
/// pending code. Admin-only.
#[utoipa::path(
    post,
    path = "/telegram_identities/link_code",
    request_body = GenerateLinkCodeRequest,
    responses((status = 200, description = "Link code minted", body = LinkCodeResponse)),
    tag = "notifications"
)]
pub async fn generate_link_code(
    State(state): State<AppState>,
    Json(req): Json<GenerateLinkCodeRequest>,
) -> AppResult<Json<LinkCodeResponse>> {
    if req.keycloak_sub.trim().is_empty() {
        return Err(AppError::BadRequest("keycloak_sub is required".to_string()));
    }
    let code = generate_code();

    state
        .db
        .execute(Statement::from_sql_and_values(
            PG,
            "DELETE FROM telegram_identities \
             WHERE linked_keycloak_sub = $1 AND telegram_chat_id IS NULL",
            [req.keycloak_sub.clone().into()],
        ))
        .await
        .map_err(|e| AppError::Internal(format!("failed to clear pending codes: {e}")))?;

    let row = state
        .db
        .query_one(Statement::from_sql_and_values(
            PG,
            "INSERT INTO telegram_identities \
                (linked_keycloak_sub, link_code, link_code_expires_at, is_active) \
             VALUES ($1, $2, NOW() + ($3 || ' minutes')::interval, TRUE) \
             RETURNING link_code_expires_at",
            [
                req.keycloak_sub.into(),
                code.clone().into(),
                LINK_CODE_TTL_MINUTES.to_string().into(),
            ],
        ))
        .await
        .map_err(|e| AppError::Internal(format!("failed to mint link code: {e}")))?
        .ok_or_else(|| AppError::Internal("no row returned minting link code".to_string()))?;

    let expires_at: DateTime<Utc> = row
        .try_get("", "link_code_expires_at")
        .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(LinkCodeResponse { code, expires_at }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct TestSendRequest {
    /// `telegram` or `email`.
    pub channel: String,
    /// Telegram chat id (numeric) or an email address, entered in the UI for this test only.
    pub recipient: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TestResult {
    pub recipient: String,
    /// `sent` or `failed`.
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

/// Send a one-off test message through a channel to a single UI-entered recipient. Does not touch the
/// real subscriber list; logged to `notification_log` with `kind = "test"`. Admin-only.
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
    let subject = "River Data test notification";
    let body = "✅ This is a test notification from River Data.";

    let outcome = match req.channel.as_str() {
        "telegram" => {
            let Some(token) = state.config.telegram_bot_token.clone() else {
                return Err(AppError::BadRequest(
                    "Telegram is not configured".to_string(),
                ));
            };
            let chat_id: i64 = recipient.parse().map_err(|_| {
                AppError::BadRequest("recipient must be a numeric Telegram chat id".to_string())
            })?;
            TelegramClient::new(token).send_message(chat_id, body).await
        }
        "email" => {
            // Guard against header injection / multi-recipient smuggling in the test address.
            if !recipient.contains('@') || recipient.contains(['\n', '\r', ',', ' ']) {
                return Err(AppError::BadRequest("invalid email address".to_string()));
            }
            let Some(mailer) = build_mailer(&state.config) else {
                return Err(AppError::BadRequest("Email is not configured".to_string()));
            };
            mailer.send(&recipient, subject, body).await
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
pub struct SubscriberRow {
    pub keycloak_sub: String,
    pub email_enabled: bool,
    pub telegram_enabled: bool,
    pub is_active: bool,
    /// `unlinked` | `pending` | `linked`.
    pub telegram_status: String,
    pub subscription_overrides: i64,
}

/// Roster of everyone with notification state, subscribers and linked Telegram chats (the union, so
/// chats linked before opting in still appear). Read-only oversight. Admin-only.
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
                SELECT linked_keycloak_sub FROM telegram_identities \
             ) \
             SELECT s.keycloak_sub, \
                COALESCE(ns.email_enabled, false) AS email_enabled, \
                COALESCE(ns.telegram_enabled, true) AS telegram_enabled, \
                COALESCE(ns.is_active, true) AS is_active, \
                (SELECT COUNT(*) FROM notification_subscriptions nsub \
                   WHERE nsub.keycloak_sub = s.keycloak_sub) AS overrides, \
                ti.telegram_chat_id, ti.link_code, ti.link_code_expires_at \
             FROM subs s \
             LEFT JOIN notification_subscribers ns ON ns.keycloak_sub = s.keycloak_sub \
             LEFT JOIN LATERAL ( \
                SELECT telegram_chat_id, link_code, link_code_expires_at FROM telegram_identities ti2 \
                WHERE ti2.linked_keycloak_sub = s.keycloak_sub ORDER BY ti2.created_at DESC LIMIT 1 \
             ) ti ON true \
             ORDER BY s.keycloak_sub"
                .to_string(),
        ))
        .await?;

    let now = Utc::now();
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let chat_id: Option<i64> = r.try_get("", "telegram_chat_id").ok().flatten();
        let link_code: Option<String> = r.try_get("", "link_code").ok().flatten();
        let expires: Option<DateTime<Utc>> = r.try_get("", "link_code_expires_at").ok().flatten();
        let telegram_status = if chat_id.is_some() {
            "linked"
        } else if link_code.is_some() && expires.is_some_and(|e| e > now) {
            "pending"
        } else {
            "unlinked"
        };
        out.push(SubscriberRow {
            keycloak_sub: r.try_get("", "keycloak_sub")?,
            email_enabled: r.try_get("", "email_enabled")?,
            telegram_enabled: r.try_get("", "telegram_enabled")?,
            is_active: r.try_get("", "is_active")?,
            telegram_status: telegram_status.to_string(),
            subscription_overrides: r.try_get("", "overrides")?,
        });
    }
    Ok(Json(out))
}
