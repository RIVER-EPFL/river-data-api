use axum::{
    extract::{FromRequestParts, Request},
    http::request::Parts,
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
}

fn default_true() -> bool {
    true
}

impl Default for TokenPermissions {
    fn default() -> Self {
        Self {
            read_metadata: true,
            read_data: true,
        }
    }
}

impl TokenPermissions {
    /// Parse from a serde_json::Value, falling back to defaults on any error.
    pub fn from_json(value: &serde_json::Value) -> Self {
        serde_json::from_value(value.clone()).unwrap_or_default()
    }
}

/// Middleware that enables dual authentication: Keycloak JWT OR API token.
///
/// Runs after `KeycloakAuthLayer` in `PassthroughMode::Pass` mode.
/// Checks the Keycloak auth status first; if that failed, tries API token validation.
/// Inserts `AuthContext` into request extensions on success.
pub async fn private_auth_middleware(
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

    // Neither auth method succeeded
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
