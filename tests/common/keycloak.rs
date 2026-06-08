//! Helpers for running the in-process app against the **dev Keycloak** (reused for JWT issuance +
//! JWKS validation). Tests gate on `keycloak_reachable()` and skip when it's down, so the default
//! suite stays green without Keycloak.

use std::sync::Arc;
use std::time::Duration;

use axum_keycloak_auth::Url;
use axum_keycloak_auth::instance::{KeycloakAuthInstance, KeycloakConfig};
use river_db::common::AppState;
use sea_orm::DatabaseConnection;

/// Base URL of the Keycloak the tests use. Everything about the Keycloak location is env-overridable
/// (nothing hardcoded): `TEST_KEYCLOAK_URL` (default the dev Keycloak's host port `:8180`; the
/// watcher container sets `http://river-db-keycloak:8080/`), `TEST_KEYCLOAK_REALM` (default
/// `river-data`), `TEST_KEYCLOAK_CLIENT_ID` (default the dev public client `river-data-ui-local`,
/// which has `directAccessGrantsEnabled` for the password grant). The JWT helper and the app's
/// `KeycloakAuthInstance` MUST share the same URL+realm, or `iss`/JWKS validation fails.
#[must_use]
pub fn keycloak_base_url() -> String {
    let raw = std::env::var("TEST_KEYCLOAK_URL")
        .unwrap_or_else(|_| "http://localhost:8180/".to_string());
    if raw.ends_with('/') {
        raw
    } else {
        format!("{raw}/")
    }
}

#[must_use]
pub fn keycloak_realm() -> String {
    std::env::var("TEST_KEYCLOAK_REALM").unwrap_or_else(|_| "river-data".to_string())
}

#[must_use]
pub fn keycloak_client_id() -> String {
    std::env::var("TEST_KEYCLOAK_CLIENT_ID").unwrap_or_else(|_| "river-data-ui-local".to_string())
}

/// Whether the configured Keycloak's OIDC discovery is reachable (2s timeout). Tests skip when false.
pub async fn keycloak_reachable() -> bool {
    let url = format!(
        "{}realms/{}/.well-known/openid-configuration",
        keycloak_base_url(),
        keycloak_realm()
    );
    let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(2)).build() else {
        return false;
    };
    matches!(client.get(&url).send().await, Ok(r) if r.status().is_success())
}

/// Obtain a real JWT via the resource-owner password grant against the configured Keycloak.
pub async fn get_keycloak_jwt(username: &str, password: &str) -> String {
    let url = format!(
        "{}realms/{}/protocol/openid-connect/token",
        keycloak_base_url(),
        keycloak_realm()
    );
    let client_id = keycloak_client_id();
    let resp = reqwest::Client::new()
        .post(&url)
        .form(&[
            ("grant_type", "password"),
            ("client_id", &client_id),
            ("username", username),
            ("password", password),
        ])
        .send()
        .await
        .expect("Keycloak unreachable");
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.expect("Keycloak returned non-JSON");
    assert!(status.is_success(), "Keycloak token request failed: {body}");
    body["access_token"]
        .as_str()
        .expect("no access_token in Keycloak response")
        .to_string()
}

/// Build the in-process app WITH a real `KeycloakAuthInstance` pointing at the dev Keycloak, so
/// bearer JWTs are validated against its JWKS — exercising the Keycloak side of the auth model
/// (which the default `keycloak_auth_instance: None` harness cannot). Mirrors `src/main.rs`.
///
/// Awaits the initial OIDC discovery before returning: the `KeycloakAuthLayer`'s `poll_ready` panics
/// (axum-keycloak-auth 0.8.3) if a request arrives before discovery has started, which a no-token
/// request can otherwise race.
pub async fn build_test_app_with_keycloak(db: DatabaseConnection) -> axum::Router {
    let base = keycloak_base_url();
    let realm = keycloak_realm();
    let mut config = super::test_config();
    config.keycloak_url = Some(base.clone());
    config.keycloak_realm = Some(realm.clone());
    config.keycloak_client_id = Some(keycloak_client_id());

    let instance = Arc::new(KeycloakAuthInstance::new(
        KeycloakConfig::builder()
            .server(Url::parse(&base).expect("valid keycloak url"))
            .realm(realm)
            .build(),
    ));
    // Poll until JWKS/OIDC discovery has landed. `is_operational()` only peeks the current state, so
    // a single await can return before discovery completes — leaving the layer's `poll_ready` to race
    // (and panic) on a no-token request. Bounded so a misconfigured Keycloak can't hang the test.
    for _ in 0..100 {
        if instance.is_operational().await {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let state = AppState::new(db, config, Some(instance));
    river_db::routes::build_router(state)
}
