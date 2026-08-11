//! Keycloak user-management proxy (`/api/users`, `/api/users/search`).
//!
//! The happy-path suites run the in-process app against the **dev Keycloak** (realm `river-data`,
//! users `admin/admin` + `user/user`, service account with `view-realm`) and auto-skip when it is
//! unreachable, same harness as `tests/auth/keycloak_jwt_capability.rs`.
//!
//! The failure-path test needs no Keycloak: it drives the `list_users` handler against an
//! in-process mock that answers the token grant but 403s the role-members listing (what a service
//! account without `view-realm` gets), and asserts the handler errors instead of answering an
//! empty 200 instead of the silent-empty behaviour that masked the missing grant.

use axum::extract::{Query, State};
use river_db::common::AppState;
use river_db::routes::private::admin::users::{
    AssignRolesRequest, ListQuery, assign_roles, list_users,
};
use serial_test::serial;

use crate::common::keycloak::{
    build_test_app_with_keycloak_admin, ensure_realm_user, get_keycloak_jwt, grant_realm_role,
    keycloak_reachable, keycloak_user_id, realm_role_names,
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
    ensure_realm_user("dualrole", "dualrole", &["riverdata-admin", "riverdata-manager"]).await;
    let jwt = get_keycloak_jwt("admin", "admin").await;

    let (status, body) = crate::common::get_with_token(&app, "/api/users", &jwt).await;
    assert_eq!(status, 200, "user list must succeed: {body}");

    let users: Vec<serde_json::Value> = serde_json::from_str(&body).expect("JSON user list");
    let usernames: Vec<&str> = users.iter().filter_map(|u| u["username"].as_str()).collect();
    assert!(usernames.contains(&"admin"), "admin user missing: {usernames:?}");
    assert!(usernames.contains(&"user"), "regular user missing: {usernames:?}");

    // A user holding two levels appears in both per-role member lists; the union must dedupe.
    assert_eq!(
        usernames.iter().filter(|u| **u == "dualrole").count(),
        1,
        "users with two roles must appear once: {usernames:?}"
    );
    let dual = users.iter().find(|u| u["username"] == "dualrole").unwrap();
    let roles: Vec<&str> = dual["roles"]
        .as_array()
        .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert!(
        roles.contains(&"riverdata-admin") && roles.contains(&"riverdata-manager"),
        "dualrole roles: {roles:?}"
    );
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
    assert!(roles.contains(&"riverdata-admin"), "search roles: {roles:?}");

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

#[tokio::test]
#[serial]
async fn assign_unknown_role_rejected_and_current_roles_untouched() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    ensure_realm_user("rolevictim", "rolevictim", &["riverdata-river"]).await;
    let victim_id = keycloak_user_id("rolevictim").await;
    let jwt = get_keycloak_jwt("admin", "admin").await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/users/{victim_id}/roles"),
        &serde_json::json!({ "roles": ["riverdata-nonexistent"] }),
        &jwt,
    )
    .await;
    assert_eq!(status, 400, "unknown role must be rejected: {body}");
    assert!(
        body.contains("riverdata-nonexistent"),
        "error names the unknown role: {body}"
    );

    let (status, body) =
        crate::common::get_with_token(&app, &format!("/api/users/{victim_id}"), &jwt).await;
    assert_eq!(status, 200, "{body}");
    let user: serde_json::Value = serde_json::from_str(&body).unwrap();
    let roles: Vec<&str> = user["roles"]
        .as_array()
        .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        roles,
        vec!["riverdata-river"],
        "rejected assignment must leave existing roles untouched"
    );
}

#[tokio::test]
#[serial]
async fn level_change_keeps_roles_from_other_applications() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    ensure_realm_user("multiapp", "multiapp", &["riverdata-intern"]).await;
    grant_realm_role("multiapp", "test-external-app").await;
    let user_id = keycloak_user_id("multiapp").await;
    let jwt = get_keycloak_jwt("admin", "admin").await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/users/{user_id}/roles"),
        &serde_json::json!({ "roles": ["riverdata-river"] }),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let roles = realm_role_names("multiapp").await;
    assert!(
        roles.iter().any(|r| r == "test-external-app"),
        "unrelated realm role must survive a level change: {roles:?}"
    );
    assert!(roles.iter().any(|r| r == "riverdata-river"), "{roles:?}");
    assert!(!roles.iter().any(|r| r == "riverdata-intern"), "{roles:?}");
}

#[tokio::test]
#[serial]
async fn assigning_a_non_river_role_is_rejected() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    ensure_realm_user("levelguard", "levelguard", &["riverdata-river"]).await;
    grant_realm_role("levelguard", "test-external-app").await;
    let user_id = keycloak_user_id("levelguard").await;
    let jwt = get_keycloak_jwt("admin", "admin").await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/users/{user_id}/roles"),
        &serde_json::json!({ "roles": ["test-external-app"] }),
        &jwt,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let roles = realm_role_names("levelguard").await;
    assert!(roles.iter().any(|r| r == "riverdata-river"), "{roles:?}");
}

#[tokio::test]
#[serial]
async fn create_user_endpoint_is_removed() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    let jwt = get_keycloak_jwt("admin", "admin").await;

    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/users",
        &serde_json::json!({ "username": "should-not-exist" }),
        &jwt,
    )
    .await;
    assert_eq!(status, 405, "POST /users must no longer be routable");
}

#[tokio::test]
#[serial]
async fn list_and_search_report_identical_roles_for_same_user() {
    require_keycloak!();
    let (_db, app) = seeded_app().await;
    let jwt = get_keycloak_jwt("admin", "admin").await;

    let roles_from = |body: &str, username: &str| -> Option<Vec<String>> {
        let users: Vec<serde_json::Value> = serde_json::from_str(body).ok()?;
        let user = users.iter().find(|u| u["username"] == username)?;
        Some(
            user["roles"]
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
        )
    };

    let (status, list_body) = crate::common::get_with_token(&app, "/api/users", &jwt).await;
    assert_eq!(status, 200, "{list_body}");
    let (status, search_body) =
        crate::common::get_with_token(&app, "/api/users/search?q=user", &jwt).await;
    assert_eq!(status, 200, "{search_body}");

    let mut list_roles = roles_from(&list_body, "user").expect("user in list");
    let mut search_roles = roles_from(&search_body, "user").expect("user in search");
    list_roles.sort();
    search_roles.sort();
    assert_eq!(
        list_roles, search_roles,
        "list and search must report the same roles shape"
    );
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

/// Mock Keycloak: token grant and role listings succeed, removing role mappings fails.
async fn spawn_mock_keycloak_failing_role_delete() -> String {
    use axum::routing::{delete, get, post};
    let river_roles = || async {
        axum::Json(serde_json::json!([
            {"id": "1", "name": "riverdata-river"},
            {"id": "2", "name": "riverdata-intern"},
        ]))
    };
    let app = axum::Router::new()
        .route(
            "/realms/{realm}/protocol/openid-connect/token",
            post(|| async {
                axum::Json(serde_json::json!({"access_token": "mock-token", "expires_in": 300}))
            }),
        )
        .route("/admin/realms/{realm}/roles", get(river_roles))
        .route(
            "/admin/realms/{realm}/users/{id}/role-mappings/realm",
            get(|| async { axum::Json(serde_json::json!([{"id": "2", "name": "riverdata-intern"}])) })
                .delete(|| async {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(serde_json::json!({"error": "delete failed"})),
                    )
                }),
        )
        .route("/admin/realms/{realm}/unused", delete(|| async { "" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
#[serial]
async fn assign_roles_errors_when_role_removal_fails() {
    let db = crate::common::setup_test_db().await;
    let mock_url = spawn_mock_keycloak_failing_role_delete().await;

    let mut config = crate::common::keycloak::test_config_with_mock_keycloak(&mock_url);
    config.database_url = std::env::var("DATABASE_URL").unwrap_or_default();
    let state = AppState::new(db, config, None);

    let result = assign_roles(
        State(state),
        axum::extract::Path("some-user".to_string()),
        axum::Json(AssignRolesRequest { roles: vec!["riverdata-river".to_string()] }),
    )
    .await;
    let err = result.err().expect("a failed role removal must not report success");
    assert!(
        format!("{err:?}").contains("500"),
        "error should carry the Keycloak status: {err:?}"
    );
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
