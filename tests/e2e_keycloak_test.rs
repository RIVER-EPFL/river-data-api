//! End-to-end tests against the running docker compose stack with Keycloak.
//!
//! These tests are `#[ignore]` by default — they require:
//!   docker compose --profile auth up -d   # in river-data-ui/
//!
//! Run with:
//!   cargo test --test e2e_keycloak_test -- --ignored --test-threads=1
//!
//! Why we need these: in-process tests can only verify that API tokens get rejected
//! by `require_admin`. They can't verify the Keycloak acceptance path because the
//! test harness passes `keycloak_auth_instance: None`. These tests confirm that a real
//! Keycloak `riverdata-admin` JWT does in fact reach admin endpoints, and a regular
//! `riverdata-user` JWT does not.

use serde_json::Value;

const KEYCLOAK_TOKEN_URL: &str =
    "http://localhost:8089/realms/river-data/protocol/openid-connect/token";
const API_BASE: &str = "http://localhost:3005";

async fn get_keycloak_jwt(client: &reqwest::Client, username: &str, password: &str) -> String {
    let resp = client
        .post(KEYCLOAK_TOKEN_URL)
        .form(&[
            ("grant_type", "password"),
            ("client_id", "river-data-ui-local"),
            ("username", username),
            ("password", password),
        ])
        .send()
        .await
        .expect("Keycloak unreachable (did you `docker compose --profile auth up -d`?)");
    let status = resp.status();
    let body: Value = resp.json().await.expect("Keycloak returned non-JSON");
    assert!(status.is_success(), "Keycloak token request failed: {body}");
    body["access_token"]
        .as_str()
        .expect("no access_token in Keycloak response")
        .to_string()
}

async fn api_get(client: &reqwest::Client, path: &str, token: Option<&str>) -> u16 {
    let url = format!("{API_BASE}{path}");
    let mut req = client.get(&url);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    req.send().await.expect("API unreachable").status().as_u16()
}

async fn api_post(
    client: &reqwest::Client,
    path: &str,
    body: &Value,
    token: Option<&str>,
) -> u16 {
    let url = format!("{API_BASE}{path}");
    let mut req = client.post(&url).json(body);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    req.send().await.expect("API unreachable").status().as_u16()
}

#[tokio::test]
#[ignore]
async fn keycloak_admin_can_reach_admin_routes() {
    let client = reqwest::Client::new();
    let jwt = get_keycloak_jwt(&client, "admin", "admin").await;

    let status = api_get(&client, "/api/v1/tokens", Some(&jwt)).await;
    assert!(
        (200..=299).contains(&status),
        "admin Keycloak JWT on /api/v1/tokens got {status} (expected 2xx)"
    );

    let status = api_get(&client, "/api/v1/sync_service_credentials", Some(&jwt)).await;
    assert!(
        (200..=299).contains(&status),
        "admin Keycloak JWT on /sync_service_credentials got {status}"
    );
}

#[tokio::test]
#[ignore]
async fn keycloak_user_cannot_reach_admin_routes() {
    let client = reqwest::Client::new();
    let jwt = get_keycloak_jwt(&client, "user", "user").await;

    let status = api_get(&client, "/api/v1/tokens", Some(&jwt)).await;
    assert_eq!(
        status, 403,
        "non-admin Keycloak JWT on /api/v1/tokens got {status} (expected 403)"
    );

    let status = api_post(
        &client,
        "/api/v1/sync/credentials",
        &serde_json::json!({"name": "blocked"}),
        Some(&jwt),
    )
    .await;
    assert_eq!(
        status, 403,
        "non-admin Keycloak JWT on POST /sync/credentials got {status} (expected 403)"
    );
}

#[tokio::test]
#[ignore]
async fn keycloak_user_can_read_metadata() {
    let client = reqwest::Client::new();
    let jwt = get_keycloak_jwt(&client, "user", "user").await;

    let status = api_get(&client, "/api/v1/projects", Some(&jwt)).await;
    assert!(
        (200..=299).contains(&status),
        "non-admin Keycloak JWT must still read metadata, got {status}"
    );

    let status = api_get(&client, "/api/v1/search?q=site", Some(&jwt)).await;
    assert!(
        (200..=299).contains(&status),
        "non-admin Keycloak JWT must still search, got {status}"
    );
}

#[tokio::test]
#[ignore]
async fn anonymous_blocked_from_admin_routes_returns_401() {
    let client = reqwest::Client::new();
    let status = api_get(&client, "/api/v1/tokens", None).await;
    assert_eq!(status, 401, "anonymous on admin route should be 401, got {status}");
}

#[tokio::test]
#[ignore]
async fn public_endpoints_work_without_keycloak() {
    let client = reqwest::Client::new();
    let status = api_get(&client, "/api/v1/public", None).await;
    assert!(
        (200..=299).contains(&status),
        "public discovery must work without auth, got {status}"
    );
}

#[tokio::test]
#[ignore]
async fn keycloak_admin_can_post_sync_credentials() {
    // The hardest path to test in-process: a real Keycloak admin JWT is the ONLY thing
    // that should pass require_admin on POST /sync/credentials. In-process tests prove
    // the deny side (no token can pass); this proves the allow side.
    let client = reqwest::Client::new();
    let jwt = get_keycloak_jwt(&client, "admin", "admin").await;

    let status = api_post(
        &client,
        "/api/v1/sync/credentials",
        &serde_json::json!({"name": "e2e-test-cred"}),
        Some(&jwt),
    )
    .await;
    // 200/201 = created; the e2e test isn't responsible for cleanup, so we accept any
    // 2xx outcome and rely on the test DB being recycled by `docker compose --profile test`.
    // 409 (conflict on duplicate name) is also acceptable on re-runs.
    assert!(
        (200..=299).contains(&status) || status == 409,
        "admin JWT POST /sync/credentials got {status} (expected 2xx or 409)"
    );
}
