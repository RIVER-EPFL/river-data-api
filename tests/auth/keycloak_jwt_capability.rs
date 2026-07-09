//! Keycloak auth tests — the in-process app validating REAL JWTs from the **dev Keycloak**.
//!
//! These reuse the dev Keycloak (realm `river-data`; users `admin/admin` + `user/user`) for token
//! issuance and JWKS validation, while running the API in-process against the **test DB**. They
//! cover the Keycloak side of the capability model that the default `keycloak_auth_instance: None`
//! harness can't: `require_admin` accepts a real `riverdata-admin` JWT, a River user is denied
//! admin + `write_metadata` but allowed `write_data`, anonymous is 401, and the public tier is open.
//!
//! They auto-skip when Keycloak is unreachable, so the default `cargo test` stays green without it.
//! Run with the dev stack up (Keycloak on :8180):
//!   DATABASE_URL=… cargo test --test auth -- --test-threads=1
//! Override the Keycloak URL with `TEST_KEYCLOAK_URL` (the watcher container uses
//! `http://river-db-keycloak:8080/`).


use crate::common::fixtures::{GLOBAL_PARAM_TEMP_ID, PROJECT_ID, SITE1_ID};
use crate::common::keycloak::{
    build_test_app_with_keycloak, get_keycloak_jwt, grant_project, keycloak_reachable,
    keycloak_user_id,
};
use serial_test::serial;

/// Skip-guard: prints and returns when Keycloak isn't reachable. libtest has no runtime-skip, so a
/// skipped test reports as `passed`; the `eprintln!` makes that visible.
macro_rules! require_keycloak {
    () => {
        if !keycloak_reachable().await {
            eprintln!(
                "SKIP: keycloak unreachable (start the dev stack, or set TEST_KEYCLOAK_URL)"
            );
            return;
        }
    };
}

async fn seeded_app() -> (sea_orm::DatabaseConnection, axum::Router) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = build_test_app_with_keycloak(db.clone()).await;
    (db, app)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[tokio::test]
#[serial]
async fn keycloak_admin_can_reach_admin_routes() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    let jwt = get_keycloak_jwt("admin", "admin").await;

    let (s, _) = crate::common::get_with_token(&app, "/api/tokens", &jwt).await;
    assert_eq!(s, 200, "admin JWT must reach /api/tokens");
    let (s, _) = crate::common::get_with_token(&app, "/api/sync_service_credentials", &jwt).await;
    assert_eq!(s, 200, "admin JWT must reach /api/sync_service_credentials");
    // The new admin-only forensic audit log is reachable for a real admin JWT.
    let (s, _) = crate::common::get_with_token(&app, "/api/api_token_audit_logs", &jwt).await;
    assert_eq!(s, 200, "admin JWT must reach the audit log");
}

#[tokio::test]
#[serial]
async fn keycloak_user_cannot_reach_admin_routes() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    let jwt = get_keycloak_jwt("user", "user").await;

    let (s, _) = crate::common::get_with_token(&app, "/api/tokens", &jwt).await;
    assert_eq!(s, 403, "non-admin JWT on /api/tokens must be 403");
    let (s, _) = crate::common::post_json_with_token(
        &app,
        "/api/sync/credentials",
        &serde_json::json!({"name": "blocked"}),
        &jwt,
    )
    .await;
    assert_eq!(s, 403, "non-admin JWT on POST /sync/credentials must be 403");
    let (s, _) = crate::common::get_with_token(&app, "/api/api_token_audit_logs", &jwt).await;
    assert_eq!(s, 403, "non-admin JWT must not read the audit log");
}

#[tokio::test]
#[serial]
async fn keycloak_user_can_read_metadata() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    let jwt = get_keycloak_jwt("user", "user").await;

    let (s, _) = crate::common::get_with_token(&app, "/api/projects", &jwt).await;
    assert_eq!(s, 200, "non-admin JWT must read metadata");
    let (s, _) = crate::common::get_with_token(&app, "/api/search?q=Station", &jwt).await;
    assert_eq!(s, 200, "non-admin JWT must search");
}

#[tokio::test]
#[serial]
async fn keycloak_capability_mapping_user_vs_admin() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    let user = get_keycloak_jwt("user", "user").await;
    let admin = get_keycloak_jwt("admin", "admin").await;
    // Grant `user` the seed project up front (before its first request, so the grants cache loads the
    // granted set, not an empty one). Capability denials below are independent of the grant.
    grant_project(&_db, &keycloak_user_id("user").await, PROJECT_ID).await;

    // write_metadata is Keycloak-Administrator-only: a non-admin user is denied a CRUD mutation...
    let param = serde_json::json!({
        "code": "kc_new", "name": "KC New", "default_units": "x",
        "category": "measurement", "aliases": []
    });
    let (s, _) = crate::common::post_json_with_token(&app, "/api/parameters", &param, &user).await;
    assert_eq!(s, 403, "a non-admin Keycloak user must be denied write_metadata");
    // ...but the admin is allowed (auth passes; a 2xx confirms the create).
    let (s, body) = crate::common::post_json_with_token(&app, "/api/parameters", &param, &admin).await;
    assert!((200..300).contains(&s), "admin must be allowed write_metadata: {body}");

    // write_data is a River capability, reachable inside the granted project (granted up front).
    let t = now_rfc3339();
    let batch = serde_json::json!({
        "readings": [{ "site_id": SITE1_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": t, "raw_value": 1.0 }]
    });
    let (s, body) = crate::common::post_json_with_token(&app, "/api/readings/batch", &batch, &user).await;
    assert_eq!(s, 200, "a granted River user has write_data: {body}");
}

#[tokio::test]
#[serial]
async fn anonymous_blocked_from_admin_routes_returns_401() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    let (s, _) = crate::common::get(&app, "/api/tokens").await;
    assert_eq!(s, 401, "anonymous on an admin route must be 401");
}

#[tokio::test]
#[serial]
async fn public_endpoints_work_without_keycloak() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    let (s, _) = crate::common::get(&app, "/api/public").await;
    assert!((200..300).contains(&s), "public discovery must work without auth");
}

#[tokio::test]
#[serial]
async fn keycloak_admin_can_post_sync_credentials() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    let jwt = get_keycloak_jwt("admin", "admin").await;

    // The allow side of `require_admin`: a real admin JWT must PASS the gate. The handler's business
    // outcome (created / conflict / validation) is incidental — assert only that auth was accepted.
    let (s, body) = crate::common::post_json_with_token(
        &app,
        "/api/sync/credentials",
        &serde_json::json!({"name": "e2e-test-cred", "service_type": "vaisala"}),
        &jwt,
    )
    .await;
    assert!(
        s != 401 && s != 403,
        "admin JWT must pass require_admin on POST /sync/credentials, got {s}: {body}"
    );
}
