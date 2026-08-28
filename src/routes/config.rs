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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationsConfig {
    pub web_push: WebPushCapability,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebPushCapability {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vapid_public_key: Option<String>,
}

pub async fn get_notifications_config(State(state): State<AppState>) -> impl IntoResponse {
    let config = &state.config;
    Json(NotificationsConfig {
        web_push: WebPushCapability {
            available: config.web_push_configured(),
            vapid_public_key: config.vapid_public_key.clone(),
        },
    })
}
