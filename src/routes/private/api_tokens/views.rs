use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use sea_orm::{
    ActiveModelTrait, ConnectionTrait, EntityTrait, FromQueryResult, IntoActiveModel, QueryOrder,
    QuerySelect, Set, Statement,
};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use super::models::{self as model, ApiToken};
use super::service::{invalidate_token_cache, mint_api_token};
use crate::common::AppState;
use crate::error::{AppError, AppResult};

/// Revoke an API token (soft-disable). The token stops working on the next request because the
/// validation cache is invalidated here. Admin-only (mounted behind `require_admin`).
#[utoipa::path(
    post,
    path = "/api/tokens/{id}/revoke",
    params(("id" = Uuid, Path, description = "Token id")),
    responses(
        (status = 200, description = "Token revoked", body = ApiToken),
        (status = 404, description = "Token not found"),
    ),
    tag = "tokens"
)]
pub async fn revoke_token(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<ApiToken>> {
    let existing = model::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Token not found".to_string()))?;

    let mut active = existing.into_active_model();
    active.is_active = Set(false);
    let updated = active.update(&state.db).await?;

    invalidate_token_cache(&state.token_cache).await;
    Ok(Json(ApiToken::from(updated)))
}

/// Rotate an API token: mint a new secret while preserving all metadata (name, description,
/// project scope, permissions, rate limit, expiry). The previous secret stops working immediately
/// (cache invalidated); the new secret is returned once in `token`. Admin-only.
#[utoipa::path(
    post,
    path = "/api/tokens/{id}/rotate",
    params(("id" = Uuid, Path, description = "Token id")),
    responses(
        (status = 200, description = "Token rotated; new secret in `token`", body = ApiToken),
        (status = 404, description = "Token not found"),
    ),
    tag = "tokens"
)]
pub async fn rotate_token(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<ApiToken>> {
    let existing = model::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Token not found".to_string()))?;

    let minted = mint_api_token();
    let mut active = existing.into_active_model();
    // Rotation is a purely cryptographic operation: replace the secret, but preserve admin state
    // (a revoked token stays revoked, rotating it must not silently re-enable it).
    active.token_hash = Set(minted.token_hash);
    active.token_prefix = Set(minted.token_prefix);
    let updated = active.update(&state.db).await?;

    invalidate_token_cache(&state.token_cache).await;
    let mut out = ApiToken::from(updated);
    out.token = Some(minted.raw_token);
    Ok(Json(out))
}

/// One recorded use of an API token from the forensic audit log.
#[derive(Debug, Serialize, ToSchema, sea_orm::FromQueryResult)]
pub struct TokenUsageEntry {
    pub method: String,
    pub path: String,
    pub status_code: i32,
    #[schema(required)]
    pub project_scope: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Recent usage of an API token (most recent first, capped at 200) from the forensic audit log.
/// Admin-only, like all token management. Empty when auditing is disabled or the token is unused.
#[utoipa::path(
    get,
    path = "/api/tokens/{id}/usage",
    params(("id" = Uuid, Path, description = "Token id")),
    responses(
        (status = 200, description = "Recent token usage", body = [TokenUsageEntry]),
    ),
    tag = "tokens"
)]
pub async fn token_usage(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<TokenUsageEntry>>> {
    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT method, path, status_code, project_scope, created_at \
             FROM api_token_audit_log WHERE token_id = $1 \
             ORDER BY created_at DESC LIMIT 200",
            [id.into()],
        ))
        .await?;

    let entries = rows
        .iter()
        .map(|r| Ok(TokenUsageEntry::from_query_result(r, "")?))
        .collect::<AppResult<Vec<_>>>()?;

    Ok(Json(entries))
}

/// Distinct HTTP status codes present in the audit log, used to populate the admin filter dropdown
/// with the values that actually occur rather than a free-form number box.
#[derive(Debug, Serialize, ToSchema)]
pub struct AuditStatusCodes {
    pub status_codes: Vec<i32>,
}

/// Distinct `status_code` values recorded in `api_token_audit_log`, ascending. Admin-only, read-only.
#[utoipa::path(
    get,
    path = "/api/api_token_audit_logs/distinct/status_codes",
    responses(
        (status = 200, description = "Distinct status codes recorded in the audit log", body = AuditStatusCodes),
    ),
    tag = "tokens"
)]
pub async fn distinct_status_codes(
    State(state): State<AppState>,
) -> AppResult<Json<AuditStatusCodes>> {
    let status_codes = model::audit_log::Entity::find()
        .select_only()
        .column(model::audit_log::Column::StatusCode)
        .distinct()
        .order_by_asc(model::audit_log::Column::StatusCode)
        .into_tuple::<i32>()
        .all(&state.db)
        .await?;

    Ok(Json(AuditStatusCodes { status_codes }))
}
