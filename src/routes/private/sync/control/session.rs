//! Sync session token authentication.
//!
//! Two callers resolve the same row for different purposes: the dual-auth middleware grants an
//! enrolled service access to the ordinary `/api` surface, and the extractor here gates the
//! control plane and yields the caller's `service_id`. Both go through `lookup_sync_session`, so
//! the expiry check cannot be forgotten on one side, and both receive the source system the
//! service enrolled for: what a service writes provenance under is a property of its identity, not
//! a field in its requests.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::AppError;
use crate::routes::private::api_tokens::service::hash_token;
use crate::routes::private::sync::{services_model, tokens_model};

/// An authenticated sync service: who it is, and what it writes provenance under.
#[derive(Debug, Clone)]
pub struct SyncSession {
    pub service_id: Uuid,
    /// The source system the service enrolled for, from its credential. `None` on a service whose
    /// credential declares none.
    pub source_system: Option<String>,
}

/// Resolve a raw bearer token to a live sync session. Returns `None` for an unknown, malformed
/// or expired token; the caller decides whether that is a 401 or a fall-through to another
/// auth method.
pub async fn lookup_sync_session(db: &DatabaseConnection, raw_token: &str) -> Option<SyncSession> {
    if raw_token.is_empty() {
        return None;
    }

    let token_hash = hash_token(raw_token);
    // The service comes back with the token: the source system it speaks for is read on every
    // authenticated request, so it is one round trip rather than a second lookup below.
    let (token, service) = tokens_model::Entity::find()
        .filter(tokens_model::Column::TokenHash.eq(&token_hash))
        .find_also_related(services_model::Entity)
        .one(db)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "DB error looking up sync token"))
        .ok()
        .flatten()?;

    if token.expires_at.with_timezone(&chrono::Utc) < chrono::Utc::now() {
        tracing::debug!(service_id = %token.service_id, "Sync token expired");
        return None;
    }

    Some(SyncSession {
        service_id: token.service_id,
        source_system: service.and_then(|s| s.source_system),
    })
}

/// Extract the raw bearer token from an `Authorization` header value.
pub fn bearer(value: Option<&str>) -> Option<&str> {
    value.and_then(|v| v.strip_prefix("Bearer ")).map(str::trim)
}

/// The authenticated sync service behind a control plane request.
#[derive(Debug, Clone)]
pub struct SyncServiceContext {
    pub service_id: Uuid,
    pub source_system: Option<String>,
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

        let session = lookup_sync_session(&state.db, raw_token)
            .await
            .ok_or_else(|| AppError::Unauthorized("Invalid session token".to_string()))?;

        Ok(Self {
            service_id: session.service_id,
            source_system: session.source_system,
        })
    }
}
