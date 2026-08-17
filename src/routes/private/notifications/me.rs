//! Self-service notification preferences. Every handler acts strictly on the caller's own Keycloak
//! `sub` (taken from the JWT, never from the request body), so a user can only ever read or change
//! their own subscription, link their own Telegram chat, and toggle their own channels.

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
use super::views::{LinkCodeResponse, generate_code};

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;
const LINK_CODE_TTL_MINUTES: i64 = 60;

/// The caller's Keycloak `sub`, or 403 for API tokens (which have no personal identity to manage).
fn require_sub(auth: &AuthContext) -> AppResult<String> {
    auth.keycloak_sub().map(str::to_string).ok_or_else(|| {
        AppError::Forbidden("notification preferences require a Keycloak login".to_string())
    })
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TelegramLink {
    /// `unlinked` | `pending` | `linked`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_expires_at: Option<DateTime<Utc>>,
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
pub struct MyNotifications {
    /// The caller's verified Keycloak email, shown read-only (resolved live from the token).
    pub email: Option<String>,
    pub email_verified: bool,
    pub email_enabled: bool,
    pub telegram_enabled: bool,
    pub telegram: TelegramLink,
    /// Whether this link is held open against idle expiry. Administrator-settable only.
    pub expiry_exempt: bool,
    /// Explicit scope overrides; absence of a more-specific override means subscribed (default on).
    pub subscriptions: Vec<SubscriptionScope>,
}

/// Create the subscriber row for this user if it doesn't exist yet (idempotent).
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

async fn load(state: &AppState, auth: &AuthContext, sub: &str) -> AppResult<MyNotifications> {
    let row = state
        .db
        .query_one(Statement::from_sql_and_values(
            PG,
            "SELECT email_enabled, telegram_enabled FROM notification_subscribers \
             WHERE keycloak_sub = $1",
            [sub.into()],
        ))
        .await?;
    let (email_enabled, telegram_enabled) = match row {
        Some(r) => (
            r.try_get::<bool>("", "email_enabled").unwrap_or(false),
            r.try_get::<bool>("", "telegram_enabled").unwrap_or(true),
        ),
        None => (false, true),
    };

    let tg_row = state
        .db
        .query_one(Statement::from_sql_and_values(
            PG,
            "SELECT telegram_chat_id, link_code, link_code_expires_at, expiry_exempt \
             FROM telegram_identities \
             WHERE linked_keycloak_sub = $1 ORDER BY created_at DESC LIMIT 1",
            [sub.into()],
        ))
        .await?;
    let expiry_exempt = tg_row
        .as_ref()
        .and_then(|r| r.try_get::<bool>("", "expiry_exempt").ok())
        .unwrap_or(false);
    let telegram = match tg_row {
        Some(r) => {
            let chat_id: Option<i64> = r.try_get("", "telegram_chat_id").ok().flatten();
            let expires: Option<DateTime<Utc>> =
                r.try_get("", "link_code_expires_at").ok().flatten();
            if chat_id.is_some() {
                TelegramLink {
                    status: "linked",
                    code_expires_at: None,
                }
            } else if expires.is_some_and(|e| e > Utc::now()) {
                TelegramLink {
                    status: "pending",
                    code_expires_at: expires,
                }
            } else {
                TelegramLink {
                    status: "unlinked",
                    code_expires_at: None,
                }
            }
        }
        None => TelegramLink {
            status: "unlinked",
            code_expires_at: None,
        },
    };

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
        email: auth.email().map(str::to_string),
        email_verified: auth.email_verified(),
        email_enabled,
        telegram_enabled,
        telegram,
        expiry_exempt,
        subscriptions,
    })
}

/// `GET /api/notifications/me`, the caller's own preferences, link state, and subscriptions.
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
    Ok(Json(load(&state, &auth, &sub).await?))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdatePrefsRequest {
    pub email_enabled: Option<bool>,
    pub telegram_enabled: Option<bool>,
    /// Hold this Telegram link open against idle expiry. Administrators only: a self-service
    /// opt-out available to everyone would mean nothing ever expires.
    pub expiry_exempt: Option<bool>,
}

/// `PATCH /api/notifications/me`, toggle the caller's channels. Email can only be enabled when the
/// caller has a verified email claim.
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
    if req.email_enabled == Some(true) && !(auth.email_verified() && auth.email().is_some()) {
        return Err(AppError::BadRequest(
            "a verified email address is required to enable email alerts".to_string(),
        ));
    }
    if let Some(exempt) = req.expiry_exempt {
        // Rejected rather than silently ignored: a user who thinks they have pinned their link and
        // has not is worse off than one who is told no.
        if !auth.is_admin() {
            return Err(AppError::Forbidden(
                "only an administrator can exempt a Telegram link from expiry".to_string(),
            ));
        }
        state
            .db
            .execute(Statement::from_sql_and_values(
                PG,
                "UPDATE telegram_identities SET expiry_exempt = $2, updated_at = NOW() \
                 WHERE linked_keycloak_sub = $1",
                [sub.clone().into(), exempt.into()],
            ))
            .await?;
    }
    ensure_subscriber(&state, &sub).await?;
    state
        .db
        .execute(Statement::from_sql_and_values(
            PG,
            "UPDATE notification_subscribers \
             SET email_enabled = COALESCE($2, email_enabled), \
                 telegram_enabled = COALESCE($3, telegram_enabled), \
                 updated_at = NOW() \
             WHERE keycloak_sub = $1",
            [
                sub.clone().into(),
                req.email_enabled.into(),
                req.telegram_enabled.into(),
            ],
        ))
        .await?;
    Ok(Json(load(&state, &auth, &sub).await?))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SetSubscriptionsRequest {
    pub subscriptions: Vec<SubscriptionScope>,
}

/// `PUT /api/notifications/me/subscriptions`, replace the caller's scope overrides. Each override
/// must target a project, a site, or a site+parameter, and (once project access is role-scoped) lie
/// within the projects the caller can access.
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

    for s in &req.subscriptions {
        if s.project_id.is_none() && s.site_id.is_none() {
            return Err(AppError::BadRequest(
                "each subscription must target a project or a site".to_string(),
            ));
        }
        if s.parameter_id.is_some() && s.site_id.is_none() {
            return Err(AppError::BadRequest(
                "a parameter-scoped subscription must also name its site".to_string(),
            ));
        }
    }

    // Project-access guard: a member may only subscribe to projects in their grant set. The live
    // request's scope is authoritative, so derive it from the auth context rather than re-resolving.
    let accessible: Option<HashSet<Uuid>> = match auth.access_scope() {
        AccessScope::Unrestricted => None,
        AccessScope::Projects(set) => Some((*set).clone()),
    };
    if accessible.is_some() {
        for s in &req.subscriptions {
            let project = resolve_project(&state, s).await?;
            if let Some(p) = project
                && !project_allowed(&accessible, p)
            {
                return Err(AppError::Forbidden(
                    "subscription references a project you cannot access".to_string(),
                ));
            }
        }
    }

    ensure_subscriber(&state, &sub).await?;
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

    Ok(Json(load(&state, &auth, &sub).await?))
}

/// Resolve the project a subscription scope falls under (for the access guard). `None` when it can't
/// be determined (e.g. an unknown site id), which the caller treats as "skip".
async fn resolve_project(state: &AppState, s: &SubscriptionScope) -> AppResult<Option<Uuid>> {
    if let Some(p) = s.project_id {
        return Ok(Some(p));
    }
    if let Some(site) = s.site_id {
        let row = state
            .db
            .query_one(Statement::from_sql_and_values(
                PG,
                "SELECT project_id FROM sites WHERE id = $1",
                [site.into()],
            ))
            .await?;
        return Ok(row.and_then(|r| r.try_get::<Uuid>("", "project_id").ok()));
    }
    Ok(None)
}

/// `POST /api/notifications/me/link_code`, mint a one-time code bound to the caller's own sub. The
/// user sends `/start <code>` to the bot to claim it. Any prior unclaimed code for this user is
/// dropped, so there is at most one pending code.
#[utoipa::path(
    post,
    path = "/notifications/me/link_code",
    responses((status = 200, description = "Link code minted", body = LinkCodeResponse)),
    tag = "notifications"
)]
pub async fn mint_my_link_code(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> AppResult<Json<LinkCodeResponse>> {
    let sub = require_sub(&auth)?;
    let code = generate_code();

    state
        .db
        .execute(Statement::from_sql_and_values(
            PG,
            "DELETE FROM telegram_identities \
             WHERE linked_keycloak_sub = $1 AND telegram_chat_id IS NULL",
            [sub.clone().into()],
        ))
        .await?;

    let row = state
        .db
        .query_one(Statement::from_sql_and_values(
            PG,
            "INSERT INTO telegram_identities \
                (linked_keycloak_sub, link_code, link_code_expires_at, is_active) \
             VALUES ($1, $2, NOW() + ($3 || ' minutes')::interval, TRUE) \
             RETURNING link_code_expires_at",
            [
                sub.into(),
                code.clone().into(),
                LINK_CODE_TTL_MINUTES.to_string().into(),
            ],
        ))
        .await?
        .ok_or_else(|| AppError::Internal("no row returned minting link code".to_string()))?;
    let expires_at: DateTime<Utc> = row
        .try_get("", "link_code_expires_at")
        .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(LinkCodeResponse { code, expires_at }))
}

/// `DELETE /api/notifications/me/telegram`, unlink the caller's Telegram chat (removes the link rows
/// for their sub). Re-linking requires a fresh code.
#[utoipa::path(
    delete,
    path = "/notifications/me/telegram",
    responses((status = 204, description = "Unlinked")),
    tag = "notifications"
)]
pub async fn unlink_my_telegram(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> AppResult<StatusCode> {
    let sub = require_sub(&auth)?;
    state
        .db
        .execute(Statement::from_sql_and_values(
            PG,
            "DELETE FROM telegram_identities WHERE linked_keycloak_sub = $1",
            [sub.into()],
        ))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
