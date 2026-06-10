//! Keycloak user-management proxy (`/api/users`, `/api/users/search`).
//!
//! The happy-path suites run the in-process app against the **dev Keycloak** (realm `river-data`,
//! users `admin/admin` + `user/user`, service account with `view-realm`) and auto-skip when it is
//! unreachable — same harness as `tests/auth/keycloak_jwt_capability.rs`.
//!
//! The failure-path test needs no Keycloak: it drives the `list_users` handler against an
//! in-process mock that answers the token grant but 403s the role-members listing (what a service
//! account without `view-realm` gets), and asserts the handler errors instead of answering an
//! empty 200 — the silent-empty behaviour that long masked the missing grant in production.

use axum::extract::{Query, State};
use river_db::common::AppState;
use river_db::routes::private::admin::users::{ListQuery, list_users};
use serial_test::serial;

use crate::common::keycloak::{
    build_test_app_with_keycloak_admin, get_keycloak_jwt, keycloak_reachable,
};

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
    let app = build_test_app_with_keycloak_admin(db.clone()).await;
    (db, app)
}

#[tokio::test]
#[serial]
async fn list_users_returns_role_holders_deduped() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    let jwt = get_keycloak_jwt("admin", "admin").await;

    let (status, body) = crate::common::get_with_token(&app, "/api/users", &jwt).await;
    assert_eq!(status, 200, "user list must succeed: {body}");

    let users: Vec<serde_json::Value> = serde_json::from_str(&body).expect("JSON user list");
    let usernames: Vec<&str> = users.iter().filter_map(|u| u["username"].as_str()).collect();
    assert!(usernames.contains(&"admin"), "admin user missing: {usernames:?}");
    assert!(usernames.contains(&"user"), "regular user missing: {usernames:?}");

    // `admin` holds both riverdata roles, so the per-role fetches each return it — the union
    // must dedupe by id.
    assert_eq!(
        usernames.iter().filter(|u| **u == "admin").count(),
        1,
        "users with both roles must appear once"
    );

    let admin = users.iter().find(|u| u["username"] == "admin").unwrap();
    let roles: Vec<&str> = admin["roles"]
        .as_array()
        .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert!(roles.contains(&"riverdata-admin"), "admin roles: {roles:?}");
}

#[tokio::test]
#[serial]
async fn search_users_matches_directory_with_roles() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    let jwt = get_keycloak_jwt("admin", "admin").await;

    let (status, body) = crate::common::get_with_token(&app, "/api/users/search?q=admin", &jwt).await;
    assert_eq!(status, 200, "search must succeed: {body}");
    let users: Vec<serde_json::Value> = serde_json::from_str(&body).expect("JSON search results");
    let admin = users
        .iter()
        .find(|u| u["username"] == "admin")
        .unwrap_or_else(|| panic!("admin not in search results: {users:?}"));
    let roles: Vec<&str> = admin["roles"]
        .as_array()
        .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert!(roles.contains(&"riverdata-user"), "search roles: {roles:?}");

    let (status, body) =
        crate::common::get_with_token(&app, "/api/users/search?q=no-such-account-xyz", &jwt).await;
    assert_eq!(status, 200);
    let users: Vec<serde_json::Value> = serde_json::from_str(&body).expect("JSON search results");
    assert!(users.is_empty(), "expected no matches: {users:?}");
}

#[tokio::test]
#[serial]
async fn non_admin_cannot_reach_user_management() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    let jwt = get_keycloak_jwt("user", "user").await;

    let (status, _) = crate::common::get_with_token(&app, "/api/users", &jwt).await;
    assert_eq!(status, 403);
    let (status, _) = crate::common::get_with_token(&app, "/api/users/search?q=a", &jwt).await;
    assert_eq!(status, 403);
}

/// Mock Keycloak: token grant succeeds, role-members listing is 403 (service account without
/// `view-realm`).
async fn spawn_mock_keycloak_forbidding_role_users() -> String {
    use axum::routing::{get, post};
    let app = axum::Router::new()
        .route(
            "/realms/{realm}/protocol/openid-connect/token",
            post(|| async {
                axum::Json(serde_json::json!({"access_token": "mock-token", "expires_in": 300}))
            }),
        )
        .route(
            "/admin/realms/{realm}/roles/{role}/users",
            get(|| async {
                (
                    axum::http::StatusCode::FORBIDDEN,
                    axum::Json(serde_json::json!({"error": "unknown_error"})),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
#[serial]
async fn list_users_errors_when_role_listing_forbidden() {
    let db = crate::common::setup_test_db().await;
    let mock_url = spawn_mock_keycloak_forbidding_role_users().await;

    let mut config = crate::common::keycloak::test_config_with_mock_keycloak(&mock_url);
    config.database_url = std::env::var("DATABASE_URL").unwrap_or_default();
    let state = AppState::new(db, config, None);

    let result = list_users(State(state), Query(ListQuery { range: None, filter: None })).await;
    let err = result.err().expect("a forbidden role listing must error, not return empty 200");
    assert!(
        format!("{err:?}").contains("403"),
        "error should carry the Keycloak status: {err:?}"
    );
}
