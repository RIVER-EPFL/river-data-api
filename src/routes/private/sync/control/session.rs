//! Sync session token authentication.
//!
//! Two callers resolve the same row for different purposes: the dual-auth middleware grants an
//! enrolled service access to the ordinary `/api` surface, and the extractor here gates the
//! control plane and yields the caller's `service_id`. Both go through `lookup_sync_session`, so
//! the expiry check cannot be forgotten on one side.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::AppError;
use crate::routes::private::api_tokens::service::hash_token;
use crate::routes::private::sync::tokens_model;

/// Resolve a raw bearer token to a live sync session. Returns `None` for an unknown, malformed
/// or expired token; the caller decides whether that is a 401 or a fall-through to another
/// auth method.
pub async fn lookup_sync_session(
    db: &DatabaseConnection,
    raw_token: &str,
) -> Option<tokens_model::Model> {
    if raw_token.is_empty() {
        return None;
    }

    let token_hash = hash_token(raw_token);
    let token = tokens_model::Entity::find()
        .filter(tokens_model::Column::TokenHash.eq(&token_hash))
        .one(db)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "DB error looking up sync token"))
        .ok()
        .flatten()?;

    if token.expires_at.with_timezone(&chrono::Utc) < chrono::Utc::now() {
        tracing::debug!(service_id = %token.service_id, "Sync token expired");
        return None;
    }

    Some(token)
}

/// Extract the raw bearer token from an `Authorization` header value.
pub fn bearer(value: Option<&str>) -> Option<&str> {
    value.and_then(|v| v.strip_prefix("Bearer ")).map(str::trim)
}

/// The authenticated sync service behind a control plane request.
#[derive(Debug, Clone)]
pub struct SyncServiceContext {
    pub service_id: Uuid,
}

impl FromRequestParts<AppState> for SyncServiceContext {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let raw_token = bearer(
            parts
                .headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
        )
        .filter(|t| !t.is_empty())
        .ok_or_else(|| AppError::Unauthorized("Bearer token required".to_string()))?;

        let token = lookup_sync_session(&state.db, raw_token)
            .await
            .ok_or_else(|| AppError::Unauthorized("Invalid session token".to_string()))?;

        Ok(Self {
            service_id: token.service_id,
        })
    }
}
