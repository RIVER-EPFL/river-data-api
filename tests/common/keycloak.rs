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
    let raw =
        std::env::var("TEST_KEYCLOAK_URL").unwrap_or_else(|_| "http://localhost:8180/".to_string());
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

/// Service-account client for the Keycloak admin proxy (`/users`, `/roles`). Defaults match the
/// dev realm import (`keycloak-realm-dev.json`): a confidential client whose service account holds
/// the `realm-management` roles (`manage-users`, `view-users`, `query-users`, `view-realm`).
#[must_use]
pub fn keycloak_admin_client_id() -> String {
    std::env::var("TEST_KEYCLOAK_ADMIN_CLIENT_ID")
        .unwrap_or_else(|_| "river-data-api-local".to_string())
}

#[must_use]
pub fn keycloak_admin_client_secret() -> String {
    std::env::var("TEST_KEYCLOAK_ADMIN_CLIENT_SECRET")
        .unwrap_or_else(|_| "river-data-api-local-secret".to_string())
}

/// A test `Config` with the Keycloak admin proxy pointed at an in-test mock server (no auth
/// instance, no real Keycloak). For driving the admin proxy handlers' failure paths directly.
#[must_use]
pub fn test_config_with_mock_keycloak(mock_url: &str) -> river_db::config::Config {
    let mut config = super::test_config();
    config.keycloak_url = Some(mock_url.to_string());
    config.keycloak_realm = Some("mock".to_string());
    config.keycloak_admin_client_id = Some("mock-client".to_string());
    config.keycloak_admin_client_secret = Some("mock-secret".to_string());
    config
}

/// Whether the configured Keycloak's OIDC discovery is reachable (2s timeout). Tests skip when false.
pub async fn keycloak_reachable() -> bool {
    let url = format!(
        "{}realms/{}/.well-known/openid-configuration",
        keycloak_base_url(),
        keycloak_realm()
    );
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    matches!(client.get(&url).send().await, Ok(r) if r.status().is_success())
}

/// Gate a Keycloak-dependent test: true to proceed, false to skip.
///
/// Skipping reports as a pass, so a CI run with no Keycloak would silently assert nothing. Setting
/// `REQUIRE_KEYCLOAK` turns the skip into a panic, which is what makes "the e2e suite runs in CI"
/// mean anything. Left unset locally so a bare `cargo test` still works without the dev stack.
pub async fn require_keycloak_or_skip(test_name: &str) -> bool {
    if keycloak_reachable().await {
        return true;
    }
    assert!(
        std::env::var("REQUIRE_KEYCLOAK").is_err(),
        "REQUIRE_KEYCLOAK is set but Keycloak at {} is unreachable, so {test_name} cannot run",
        keycloak_base_url()
    );
    eprintln!(
        "skipping {test_name}: Keycloak unreachable at {}",
        keycloak_base_url()
    );
    false
}

/// Obtain a service-account admin token for the dev Keycloak's admin REST API (client credentials
/// grant with the same confidential client the app's admin proxy uses).
async fn get_keycloak_admin_token() -> String {
    let url = format!(
        "{}realms/{}/protocol/openid-connect/token",
        keycloak_base_url(),
        keycloak_realm()
    );
    let resp = reqwest::Client::new()
        .post(&url)
        .form(&[
            ("grant_type", "client_credentials"),
            ("client_id", &keycloak_admin_client_id()),
            ("client_secret", &keycloak_admin_client_secret()),
        ])
        .send()
        .await
        .expect("Keycloak unreachable");
    let body: serde_json::Value = resp.json().await.expect("Keycloak returned non-JSON");
    body["access_token"]
        .as_str()
        .expect("no access_token in Keycloak admin response")
        .to_string()
}

/// Idempotently ensure a realm user exists with the given password and EXACTLY the given
/// `riverdata-*` realm roles (any other riverdata mappings are removed; unrelated realm roles are
/// left alone). Lets tests provision fixture identities, e.g. a role-less user for the access
/// gate, without a realm re-import (Keycloak's `--import-realm` skips existing realms).
pub async fn ensure_realm_user(username: &str, password: &str, river_roles: &[&str]) {
    let admin_token = get_keycloak_admin_token().await;
    let base = format!("{}admin/realms/{}", keycloak_base_url(), keycloak_realm());
    let client = reqwest::Client::new();

    // A complete representation: missing profile fields or pending required actions make the
    // password grant fail with "Account is not fully set up".
    let representation = serde_json::json!({
        "username": username,
        "enabled": true,
        "emailVerified": true,
        "email": format!("{username}@test.local"),
        "firstName": "Test",
        "lastName": username,
        "requiredActions": [],
        "credentials": [{"type": "password", "value": password, "temporary": false}],
    });
    let create = client
        .post(format!("{base}/users"))
        .bearer_auth(&admin_token)
        .json(&representation)
        .send()
        .await
        .expect("Keycloak unreachable");
    assert!(
        create.status().is_success() || create.status() == reqwest::StatusCode::CONFLICT,
        "user create failed: {}",
        create.status()
    );

    let found: serde_json::Value = client
        .get(format!("{base}/users?username={username}&exact=true"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .expect("Keycloak unreachable")
        .json()
        .await
        .expect("non-JSON user search");
    let user_id = found[0]["id"]
        .as_str()
        .expect("user not found after create")
        .to_string();

    // Existing user: converge on the fixture representation (clears stale required actions) and
    // make sure the password matches.
    client
        .put(format!("{base}/users/{user_id}"))
        .bearer_auth(&admin_token)
        .json(&representation)
        .send()
        .await
        .expect("Keycloak unreachable");
    client
        .put(format!("{base}/users/{user_id}/reset-password"))
        .bearer_auth(&admin_token)
        .json(&serde_json::json!({"type": "password", "value": password, "temporary": false}))
        .send()
        .await
        .expect("Keycloak unreachable");

    let current: serde_json::Value = client
        .get(format!("{base}/users/{user_id}/role-mappings/realm"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .expect("Keycloak unreachable")
        .json()
        .await
        .expect("non-JSON role mappings");
    let stale: Vec<serde_json::Value> = current
        .as_array()
        .map(|roles| {
            roles
                .iter()
                .filter(|r| {
                    r["name"]
                        .as_str()
                        .is_some_and(|n| n.starts_with("riverdata-") && !river_roles.contains(&n))
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if !stale.is_empty() {
        client
            .delete(format!("{base}/users/{user_id}/role-mappings/realm"))
            .bearer_auth(&admin_token)
            .json(&stale)
            .send()
            .await
            .expect("Keycloak unreachable");
    }

    let mut to_add = Vec::new();
    for role in river_roles {
        to_add.push(ensure_realm_role(&client, &admin_token, &base, role).await);
    }
    if !to_add.is_empty() {
        client
            .post(format!("{base}/users/{user_id}/role-mappings/realm"))
            .bearer_auth(&admin_token)
            .json(&to_add)
            .send()
            .await
            .expect("Keycloak unreachable");
    }
}

/// Idempotently ensure a realm role exists, returning its full representation. The new `riverdata-*`
/// levels may not yet exist in the live dev realm (they're created out of band at ship time); tests
/// create them on demand so they don't depend on realm-import ordering.
async fn ensure_realm_role(
    client: &reqwest::Client,
    admin_token: &str,
    base: &str,
    role: &str,
) -> serde_json::Value {
    let get = || async {
        client
            .get(format!("{base}/roles/{role}"))
            .bearer_auth(admin_token)
            .send()
            .await
            .expect("Keycloak unreachable")
    };
    let existing = get().await;
    if existing.status().is_success() {
        return existing.json().await.expect("non-JSON role");
    }
    let created = client
        .post(format!("{base}/roles"))
        .bearer_auth(admin_token)
        .json(&serde_json::json!({ "name": role }))
        .send()
        .await
        .expect("Keycloak unreachable");
    assert!(
        created.status().is_success() || created.status() == reqwest::StatusCode::CONFLICT,
        "realm role {role} create failed: {}",
        created.status()
    );
    let rep: serde_json::Value = get().await.json().await.expect("non-JSON role");
    assert!(
        rep["id"].is_string(),
        "realm role {role} missing after create"
    );
    rep
}

/// Master-realm admin token. Creating realm roles is beyond the API service account's grants, so
/// fixtures that need a non-river realm role go through the dev Keycloak's own admin account.
async fn master_admin_token() -> String {
    let user = std::env::var("TEST_KEYCLOAK_MASTER_USER").unwrap_or_else(|_| "admin".to_string());
    let password =
        std::env::var("TEST_KEYCLOAK_MASTER_PASSWORD").unwrap_or_else(|_| "admin".to_string());
    let body: serde_json::Value = reqwest::Client::new()
        .post(format!(
            "{}realms/master/protocol/openid-connect/token",
            keycloak_base_url()
        ))
        .form(&[
            ("client_id", "admin-cli"),
            ("grant_type", "password"),
            ("username", user.as_str()),
            ("password", password.as_str()),
        ])
        .send()
        .await
        .expect("Keycloak unreachable")
        .json()
        .await
        .expect("non-JSON master token response");
    body["access_token"]
        .as_str()
        .expect("no access_token in master admin response")
        .to_string()
}

/// Grant a realm role that is not a river access level, creating it if needed.
pub async fn grant_realm_role(username: &str, role: &str) {
    let admin_token = master_admin_token().await;
    let base = format!("{}admin/realms/{}", keycloak_base_url(), keycloak_realm());
    let client = reqwest::Client::new();
    let rep = ensure_realm_role(&client, &admin_token, &base, role).await;
    let user_id = keycloak_user_id(username).await;
    client
        .post(format!("{base}/users/{user_id}/role-mappings/realm"))
        .bearer_auth(&admin_token)
        .json(&serde_json::json!([rep]))
        .send()
        .await
        .expect("Keycloak unreachable");
}

/// Every realm role mapped to a user, river and non-river alike.
pub async fn realm_role_names(username: &str) -> Vec<String> {
    let admin_token = get_keycloak_admin_token().await;
    let base = format!("{}admin/realms/{}", keycloak_base_url(), keycloak_realm());
    let user_id = keycloak_user_id(username).await;
    let mappings: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/users/{user_id}/role-mappings/realm"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .expect("Keycloak unreachable")
        .json()
        .await
        .expect("non-JSON role mappings");
    mappings
        .as_array()
        .map(|roles| {
            roles
                .iter()
                .filter_map(|r| r["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// The Keycloak `sub` (== realm user id) for a username, via the admin API. Project grants are keyed
/// by `sub`, so tests seed grants against this. Panics if the user doesn't exist.
pub async fn keycloak_user_id(username: &str) -> String {
    let admin_token = get_keycloak_admin_token().await;
    let base = format!("{}admin/realms/{}", keycloak_base_url(), keycloak_realm());
    let found: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/users?username={username}&exact=true"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .expect("Keycloak unreachable")
        .json()
        .await
        .expect("non-JSON user search");
    found[0]["id"].as_str().expect("user not found").to_string()
}

/// Seed a project visibility grant directly (bypassing the admin endpoint) so capability tests can
/// isolate the role→capability axis from the grant axis. Idempotent.
pub async fn grant_project(db: &DatabaseConnection, sub: &str, project_id: &str) {
    use sea_orm::{ConnectionTrait, Statement};
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO user_project_grants (user_sub, project_id, granted_by) VALUES ($1, $2::uuid, 'test') \
         ON CONFLICT (user_sub, project_id) DO NOTHING",
        [sub.into(), project_id.into()],
    ))
    .await
    .expect("grant insert failed");
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
/// bearer JWTs are validated against its JWKS, exercising the Keycloak side of the auth model
/// (which the default `keycloak_auth_instance: None` harness cannot). Mirrors `src/main.rs`.
///
/// Awaits the initial OIDC discovery before returning: the `KeycloakAuthLayer`'s `poll_ready` panics
/// (axum-keycloak-auth 0.8.3) if a request arrives before discovery has started, which a no-token
/// request can otherwise race.
pub async fn build_test_app_with_keycloak(db: DatabaseConnection) -> axum::Router {
    build_test_app_with_keycloak_inner(db, false, super::test_config()).await
}

/// Like [`build_test_app_with_keycloak`], additionally configuring the Keycloak **admin proxy**
/// (service-account client credentials), so the conditional `/users` + `/roles` routes are mounted.
pub async fn build_test_app_with_keycloak_admin(db: DatabaseConnection) -> axum::Router {
    build_test_app_with_keycloak_inner(db, true, super::test_config()).await
}

/// Like [`build_test_app_with_keycloak`] with the response cache enabled, for cache tests that
/// assert on JWT-derived scope in the cache key.
pub async fn build_test_app_with_keycloak_and_cache(db: DatabaseConnection) -> axum::Router {
    build_test_app_with_keycloak_inner(db, false, super::cached_test_config()).await
}

async fn build_test_app_with_keycloak_inner(
    db: DatabaseConnection,
    with_admin_proxy: bool,
    mut config: river_db::config::Config,
) -> axum::Router {
    let base = keycloak_base_url();
    let realm = keycloak_realm();
    config.keycloak_url = Some(base.clone());
    config.keycloak_realm = Some(realm.clone());
    config.keycloak_client_id = Some(keycloak_client_id());
    if with_admin_proxy {
        config.keycloak_admin_client_id = Some(keycloak_admin_client_id());
        config.keycloak_admin_client_secret = Some(keycloak_admin_client_secret());
    }

    let instance = Arc::new(KeycloakAuthInstance::new(
        KeycloakConfig::builder()
            .server(Url::parse(&base).expect("valid keycloak url"))
            .realm(realm)
            .build(),
    ));
    // Poll until JWKS/OIDC discovery has landed. `is_operational()` only peeks the current state, so
    // a single await can return before discovery completes, leaving the layer's `poll_ready` to race
    // (and panic) on a no-token request. Bounded so a misconfigured Keycloak can't hang the test.
    for _ in 0..100 {
        if instance.is_operational().await {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let state = AppState::new(db, config, Some(instance));
    // The non-Keycloak builders all start the tracked-job worker; without it here any Keycloak
    // test that triggers a job would wait on one that is queued and never runs.
    super::spawn_test_worker(&state);
    river_db::routes::build_router(state)
}
