use axum::{
    extract::{FromRequestParts, Request},
    http::{Method, request::Parts},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::auth::Role;
use crate::error::AppError;
use crate::services::api_token::validate_bearer_token;

// Type alias for the Keycloak auth status used throughout this module.
type KcStatus = axum_keycloak_auth::KeycloakAuthStatus<
    Role,
    axum_keycloak_auth::decode::ProfileAndEmail,
>;

/// How the current request was authenticated.
#[derive(Debug, Clone)]
pub enum AuthContext {
    /// Authenticated via Keycloak JWT (admin UI, browser sessions).
    Keycloak { roles: Vec<Role> },
    /// Authenticated via API token (external scripts, curl).
    ApiToken {
        token_id: Uuid,
        permissions: TokenPermissions,
        project_scope: Option<Uuid>,
    },
}

impl AuthContext {
    fn has_role(&self, target: &Role) -> bool {
        match self {
            AuthContext::Keycloak { roles } => roles.contains(target),
            AuthContext::ApiToken { .. } => false,
        }
    }

    fn is_admin(&self) -> bool {
        self.has_role(&Role::Administrator)
    }
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
            axum_keycloak_auth::KeycloakAuthStatus::Success(token) => {
                let roles: Vec<Role> = token
                    .roles
                    .iter()
                    .map(|kr| kr.role().clone())
                    .collect();
                request
                    .extensions_mut()
                    .insert(AuthContext::Keycloak { roles });
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
        && let Some(token_model) = validate_bearer_token(&state.db, &header_value, &state.token_cache).await
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
    // needs to call regular service-tier endpoints (streams, ingest, readings/batch, etc.).
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
/// Keycloak admins and users both have read access.
pub async fn require_read_metadata(request: Request, next: Next) -> Response {
    match request.extensions().get::<AuthContext>() {
        Some(AuthContext::Keycloak { .. }) => next.run(request).await,
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
/// Keycloak admins and users both have read access.
pub async fn require_read_data(request: Request, next: Next) -> Response {
    match request.extensions().get::<AuthContext>() {
        Some(AuthContext::Keycloak { .. }) => next.run(request).await,
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
/// Only Keycloak admins can write metadata.
pub async fn require_write_metadata(request: Request, next: Next) -> Response {
    match request.extensions().get::<AuthContext>() {
        Some(ctx @ AuthContext::Keycloak { .. }) => {
            if ctx.is_admin() {
                next.run(request).await
            } else {
                AppError::Forbidden("Requires riverdata-admin role".to_string()).into_response()
            }
        }
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
/// Keycloak admins and users can write data.
pub async fn require_write_data(request: Request, next: Next) -> Response {
    match request.extensions().get::<AuthContext>() {
        Some(AuthContext::Keycloak { .. }) => next.run(request).await,
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
/// GET/HEAD → any authenticated user; mutations → requires admin role or write_metadata token.
pub async fn require_crud_permissions(request: Request, next: Next) -> Response {
    let is_read = matches!(*request.method(), Method::GET | Method::HEAD);
    match request.extensions().get::<AuthContext>() {
        Some(ctx @ AuthContext::Keycloak { .. }) => {
            if is_read || ctx.is_admin() {
                next.run(request).await
            } else {
                AppError::Forbidden("Requires riverdata-admin role for mutations".to_string())
                    .into_response()
            }
        }
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
                AuthContext::Keycloak { .. } => None,
            });
        Ok(ProjectScope(scope))
    }
}
