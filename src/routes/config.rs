use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Serialize;

use crate::common::AppState;

#[derive(Serialize)]
pub struct KeycloakConfig {
    pub url: String,
    pub realm: String,
    #[serde(rename = "clientId")]
    pub client_id: String,
}

/// Returns Keycloak configuration for the frontend, or 404 if not configured.
pub async fn get_keycloak_config(State(state): State<AppState>) -> impl IntoResponse {
    let config = &state.config;

    match (
        &config.keycloak_url,
        &config.keycloak_realm,
        &config.keycloak_client_id,
    ) {
        (Some(url), Some(realm), Some(client_id)) => Json(KeycloakConfig {
            url: url.clone(),
            realm: realm.clone(),
            client_id: client_id.clone(),
        })
        .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Which notification channels the deployment has configured (driven by env vars). The frontend uses
/// this to enable or grey-out each channel. Carries no secrets — only availability booleans, the
/// email backend kind, and the public bot username.
#[derive(Serialize)]
pub struct NotificationsConfig {
    pub telegram: TelegramCapability,
    pub email: EmailCapability,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelegramCapability {
    pub available: bool,
    /// Public bot username (no `@`), for building the `t.me/<bot>?start=<code>` link. `None` if unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_username: Option<String>,
}

#[derive(Serialize)]
pub struct EmailCapability {
    pub available: bool,
    /// `smtp` | `graph` | `disabled`.
    pub backend: String,
}

/// Notification channel capabilities for the frontend. Always answers (unlike Keycloak config, an
/// all-disabled state is valid); never returns 404 and never leaks credentials.
pub async fn get_notifications_config(State(state): State<AppState>) -> impl IntoResponse {
    let config = &state.config;
    Json(NotificationsConfig {
        telegram: TelegramCapability {
            available: config.telegram_configured(),
            bot_username: config.telegram_bot_username.clone(),
        },
        email: EmailCapability {
            available: config.email_configured(),
            backend: config.email_backend_str().to_string(),
        },
    })
}
