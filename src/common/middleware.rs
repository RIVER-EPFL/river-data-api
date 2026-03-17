use axum::{
    extract::{FromRequestParts, Request},
    http::{Method, request::Parts},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::AppError;
use crate::services::api_token::validate_bearer_token;

// Type alias for the Keycloak auth status used throughout this module.
type KcStatus = axum_keycloak_auth::KeycloakAuthStatus<
    crate::common::auth::Role,
    axum_keycloak_auth::decode::ProfileAndEmail,
>;

/// How the current request was authenticated.
#[derive(Debug, Clone)]
pub enum AuthContext {
    /// Authenticated via Keycloak JWT (admin UI, browser sessions).
    Keycloak,
    /// Authenticated via API token (external scripts, curl).
    ApiToken {
        token_id: Uuid,
        permissions: TokenPermissions,
        project_scope: Option<Uuid>,
    },
}

/// Structured permissions for API tokens.
/// Deserialized from the JSONB `permissions` column with serde defaults.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenPermissions {
    #[serde(default = "default_true")]
    pub read_metadata: bool,
    #[serde(default = "default_true")]
    pub read_data: bool,
    #[serde(default)]
    pub write_metadata: bool,
    #[serde(default)]
    pub write_data: bool,
}

fn default_true() -> bool {
    true
}

impl Default for TokenPermissions {
    fn default() -> Self {
        Self {
            read_metadata: true,
            read_data: true,
            write_metadata: false,
            write_data: false,
        }
    }
}

impl TokenPermissions {
    /// Parse from a `serde_json::Value`, falling back to defaults on any error.
    #[must_use] 
    pub fn from_json(value: &serde_json::Value) -> Self {
        serde_json::from_value(value.clone()).unwrap_or_default()
    }
}

/// Middleware that enables dual authentication: Keycloak JWT OR API token.
///
/// Runs after `KeycloakAuthLayer` in `PassthroughMode::Pass` mode.
/// Checks the Keycloak auth status first; if that failed, tries API token validation.
/// Inserts `AuthContext` into request extensions on success.
pub async fn service_auth_middleware(
    state: axum::extract::State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    // Check if Keycloak auth succeeded (inserted by KeycloakAuthLayer in Pass mode)
    if let Some(status) = request.extensions().get::<KcStatus>() {
        match status {
            axum_keycloak_auth::KeycloakAuthStatus::Success(_) => {
                request.extensions_mut().insert(AuthContext::Keycloak);
                return next.run(request).await;
            }
            axum_keycloak_auth::KeycloakAuthStatus::Failure(_) => {
                // Keycloak auth failed — fall through to try API token
            }
        }
    }

    // Try API token auth from Authorization header
    let auth_header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    if let Some(header_value) = auth_header
        && let Some(token_model) = validate_bearer_token(&state.db, &header_value).await
    {
        let permissions = TokenPermissions::from_json(&token_model.permissions);
        request.extensions_mut().insert(AuthContext::ApiToken {
            token_id: token_model.id,
            permissions,
            project_scope: token_model.project_scope,
        });
        return next.run(request).await;
    }

    // Try sync service session token as last resort.
    // The sync microservice authenticates via /api/service/sync/enroll but then
    // needs to call regular service-tier endpoints (source_mappings, readings/batch, etc.).
    let sync_header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|v| v.trim());

    if let Some(raw) = sync_header
        && !raw.is_empty()
    {
        let token_hash = crate::services::api_token::hash_token(raw);

        use crate::entity::sync_service_tokens;
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

        if let Ok(Some(token)) = sync_service_tokens::Entity::find()
            .filter(sync_service_tokens::Column::TokenHash.eq(&token_hash))
            .one(&state.db)
            .await
        {
            if token.expires_at.with_timezone(&chrono::Utc) >= chrono::Utc::now() {
                request.extensions_mut().insert(AuthContext::ApiToken {
                    token_id: token.service_id,
                    permissions: TokenPermissions {
                        read_metadata: true,
                        read_data: true,
                        write_metadata: true,
                        write_data: true,
                    },
                    project_scope: None,
                });
                return next.run(request).await;
            }
        }
    }

    // No auth method succeeded
    AppError::Unauthorized("Valid Keycloak JWT or API token required".to_string()).into_response()
}

/// Scope middleware: requires `read_metadata` permission.
/// Keycloak users pass through unconditionally.
pub async fn require_read_metadata(request: Request, next: Next) -> Response {
    match request.extensions().get::<AuthContext>() {
        Some(AuthContext::Keycloak) => next.run(request).await,
        Some(AuthContext::ApiToken { permissions, .. }) => {
            if permissions.read_metadata {
                next.run(request).await
            } else {
                AppError::Forbidden("Token lacks read_metadata permission".to_string())
                    .into_response()
            }
        }
        None => AppError::Unauthorized("Authentication required".to_string()).into_response(),
    }
}

/// Scope middleware: requires `read_data` permission.
/// Keycloak users pass through unconditionally.
pub async fn require_read_data(request: Request, next: Next) -> Response {
    match request.extensions().get::<AuthContext>() {
        Some(AuthContext::Keycloak) => next.run(request).await,
        Some(AuthContext::ApiToken { permissions, .. }) => {
            if permissions.read_data {
                next.run(request).await
            } else {
                AppError::Forbidden("Token lacks read_data permission".to_string()).into_response()
            }
        }
        None => AppError::Unauthorized("Authentication required".to_string()).into_response(),
    }
}

/// Scope middleware: requires `write_metadata` permission.
/// Keycloak users pass through unconditionally.
pub async fn require_write_metadata(request: Request, next: Next) -> Response {
    match request.extensions().get::<AuthContext>() {
        Some(AuthContext::Keycloak) => next.run(request).await,
        Some(AuthContext::ApiToken { permissions, .. }) => {
            if permissions.write_metadata {
                next.run(request).await
            } else {
                AppError::Forbidden("Token lacks write_metadata permission".to_string())
                    .into_response()
            }
        }
        None => AppError::Unauthorized("Authentication required".to_string()).into_response(),
    }
}

/// Scope middleware: requires `write_data` permission.
/// Keycloak users pass through unconditionally.
pub async fn require_write_data(request: Request, next: Next) -> Response {
    match request.extensions().get::<AuthContext>() {
        Some(AuthContext::Keycloak) => next.run(request).await,
        Some(AuthContext::ApiToken { permissions, .. }) => {
            if permissions.write_data {
                next.run(request).await
            } else {
                AppError::Forbidden("Token lacks write_data permission".to_string())
                    .into_response()
            }
        }
        None => AppError::Unauthorized("Authentication required".to_string()).into_response(),
    }
}

/// Method-aware scope middleware for `CrudCrate` routes.
/// GET/HEAD → requires `read_metadata`; all other methods → requires `write_metadata`.
/// Keycloak users pass through unconditionally.
pub async fn require_crud_permissions(request: Request, next: Next) -> Response {
    let is_read = matches!(*request.method(), Method::GET | Method::HEAD);
    match request.extensions().get::<AuthContext>() {
        Some(AuthContext::Keycloak) => next.run(request).await,
        Some(AuthContext::ApiToken { permissions, .. }) => {
            if is_read {
                if permissions.read_metadata {
                    next.run(request).await
                } else {
                    AppError::Forbidden("Token lacks read_metadata permission".to_string())
                        .into_response()
                }
            } else if permissions.write_metadata {
                next.run(request).await
            } else {
                AppError::Forbidden("Token lacks write_metadata permission".to_string())
                    .into_response()
            }
        }
        None => AppError::Unauthorized("Authentication required".to_string()).into_response(),
    }
}

/// Auth context for sync service session tokens.
#[derive(Debug, Clone)]
pub struct SyncServiceContext {
    pub service_id: Uuid,
}

/// Middleware that validates sync service session tokens.
/// These are short-lived tokens from the `sync_service_tokens` table.
pub async fn sync_service_auth_middleware(
    state: axum::extract::State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let auth_header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|v| v.trim().to_string());

    let Some(raw_token) = auth_header else {
        return AppError::Unauthorized("Bearer token required".to_string()).into_response();
    };

    if raw_token.is_empty() {
        return AppError::Unauthorized("Bearer token required".to_string()).into_response();
    }

    let token_hash = crate::services::api_token::hash_token(&raw_token);

    // Look up token in sync_service_tokens
    use crate::entity::sync_service_tokens;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let token = sync_service_tokens::Entity::find()
        .filter(sync_service_tokens::Column::TokenHash.eq(&token_hash))
        .one(&state.db)
        .await;

    let token = match token {
        Ok(Some(t)) => t,
        _ => {
            return AppError::Unauthorized("Invalid session token".to_string()).into_response();
        }
    };

    // Check expiry
    if token.expires_at.with_timezone(&chrono::Utc) < chrono::Utc::now() {
        return AppError::Unauthorized("Session token expired".to_string()).into_response();
    }

    request.extensions_mut().insert(SyncServiceContext {
        service_id: token.service_id,
    });

    next.run(request).await
}

/// Extractor that yields the project scope from `AuthContext::ApiToken`, if any.
///
/// Returns `None` for Keycloak users or unscoped API tokens.
/// Handlers use this to filter queries by project when a token is scoped.
#[derive(Debug, Clone)]
pub struct ProjectScope(pub Option<Uuid>);

impl<S: Send + Sync> FromRequestParts<S> for ProjectScope {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let scope = parts
            .extensions
            .get::<AuthContext>()
            .and_then(|ctx| match ctx {
                AuthContext::ApiToken { project_scope, .. } => *project_scope,
                AuthContext::Keycloak => None,
            });
        Ok(ProjectScope(scope))
    }
}
