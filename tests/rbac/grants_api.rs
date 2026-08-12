//! `/api/me` (the UI's identity/visibility source of truth) and the admin grant-management endpoints
//! `GET|PUT /api/users/{id}/grants`. Proves: `/api/me` reports the caller's highest level + granted
//! projects (all projects for an admin); a PUT replaces the grant set transactionally and busts the
//! short-TTL grants cache so the change gates the very next request; an emptied grant set fails closed.

use crate::common::fixtures::{PROJECT_ID, SITE1_ID};
use crate::common::keycloak::{
    build_test_app_with_keycloak_admin, ensure_realm_user, get_keycloak_jwt, keycloak_reachable,
    keycloak_user_id,
};
use serde_json::Value;
use serial_test::serial;

macro_rules! require_keycloak {
    () => {
        if !keycloak_reachable().await {
            eprintln!("SKIP: keycloak unreachable (start the dev stack, or set TEST_KEYCLOAK_URL)");
            return;
        }
    };
}

async fn seeded_admin_app() -> axum::Router {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    build_test_app_with_keycloak_admin(db).await
}

fn parse(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("bad JSON: {e}\n{body}"))
}

fn passed_auth(status: u16) -> bool {
    status != 401 && status != 403
}

#[tokio::test]
#[serial]
async fn me_reports_admin_level_and_all_projects() {
    require_keycloak!();
    let app = seeded_admin_app().await;
    let admin = get_keycloak_jwt("admin", "admin").await;

    let (s, body) = crate::common::get_with_token(&app, "/api/me", &admin).await;
    assert_eq!(s, 200, "me is reachable by any river role: {body}");
    let me = parse(&body);
    assert_eq!(me["is_admin"], true);
    assert_eq!(me["role"], "administrator");
    // An administrator is unrestricted: /api/me lists every project so the UI shows them all.
    let names: Vec<&str> = me["grants"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|g| g["project_id"].as_str())
        .collect();
    assert!(names.contains(&PROJECT_ID), "admin sees the seed project: {body}");
}

#[tokio::test]
#[serial]
async fn me_reports_river_level_and_only_granted_projects() {
    require_keycloak!();
    let app = seeded_admin_app().await;
    ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    let river1_id = keycloak_user_id("river1").await;
    let admin = get_keycloak_jwt("admin", "admin").await;

    // Grant the seed project through the admin endpoint (not a direct insert).
    let (s, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/users/{river1_id}/grants"),
        &serde_json::json!({ "project_ids": [PROJECT_ID] }),
        &admin,
    )
    .await;
    assert_eq!(s, 200, "admin sets grants: {body}");

    let river = get_keycloak_jwt("river1", "river1").await;
    let (s, body) = crate::common::get_with_token(&app, "/api/me", &river).await;
    assert_eq!(s, 200);
    let me = parse(&body);
    assert_eq!(me["is_admin"], false);
    assert_eq!(me["role"], "river");
    let grants = me["grants"].as_array().unwrap();
    assert_eq!(grants.len(), 1, "exactly the one granted project: {body}");
    assert_eq!(grants[0]["project_id"], PROJECT_ID);
    assert_eq!(grants[0]["name"], "Test River Project");
}

#[tokio::test]
#[serial]
async fn put_grants_busts_cache_and_gates_writes() {
    require_keycloak!();
    let app = seeded_admin_app().await;
    ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    let river1_id = keycloak_user_id("river1").await;
    let admin = get_keycloak_jwt("admin", "admin").await;
    let river = get_keycloak_jwt("river1", "river1").await;
    let note = serde_json::json!({ "site_id": SITE1_ID, "content": "grant check" });

    // No grant yet → the write is refused (and this caches the empty grant set).
    let (s, _) = crate::common::post_json_with_token(&app, "/api/notes", &note, &river).await;
    assert_eq!(s, 403, "ungranted river user cannot write");

    // Granting busts the cache, so the very next request sees the new grant.
    let (s, _) = crate::common::put_json_with_token(
        &app,
        &format!("/api/users/{river1_id}/grants"),
        &serde_json::json!({ "project_ids": [PROJECT_ID] }),
        &admin,
    )
    .await;
    assert_eq!(s, 200);
    let (s, body) = crate::common::post_json_with_token(&app, "/api/notes", &note, &river).await;
    assert!(passed_auth(s), "granted river user writes: {s} {body}");

    // Revoking (empty set) busts the cache again → fail closed.
    let (s, _) = crate::common::put_json_with_token(
        &app,
        &format!("/api/users/{river1_id}/grants"),
        &serde_json::json!({ "project_ids": [] }),
        &admin,
    )
    .await;
    assert_eq!(s, 200);
    let (s, _) = crate::common::post_json_with_token(&app, "/api/notes", &note, &river).await;
    assert_eq!(s, 403, "revoked river user is refused again");
}

#[tokio::test]
#[serial]
async fn list_grants_returns_named_projects() {
    require_keycloak!();
    let app = seeded_admin_app().await;
    ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    let river1_id = keycloak_user_id("river1").await;
    let admin = get_keycloak_jwt("admin", "admin").await;

    let path = format!("/api/users/{river1_id}/grants");
    let (s, body) = crate::common::get_with_token(&app, &path, &admin).await;
    assert_eq!(s, 200, "list starts empty: {body}");
    assert_eq!(parse(&body).as_array().unwrap().len(), 0);

    crate::common::put_json_with_token(
        &app,
        &path,
        &serde_json::json!({ "project_ids": [PROJECT_ID] }),
        &admin,
    )
    .await;

    let (s, body) = crate::common::get_with_token(&app, &path, &admin).await;
    assert_eq!(s, 200);
    let grants = parse(&body);
    let grants = grants.as_array().unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0]["project_id"], PROJECT_ID);
    assert_eq!(grants[0]["name"], "Test River Project");
}

/// A non-admin (even a manager) cannot read or write another user's grants, grant management is
/// Administrator-only, like all of `/api/users`.
#[tokio::test]
#[serial]
async fn grant_management_is_admin_only() {
    require_keycloak!();
    let app = seeded_admin_app().await;
    ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    let manager = get_keycloak_jwt("manager1", "manager1").await;
    let some_id = keycloak_user_id("manager1").await;

    let path = format!("/api/users/{some_id}/grants");
    let (s, _) = crate::common::get_with_token(&app, &path, &manager).await;
    assert_eq!(s, 403, "manager cannot read grants");
    let (s, _) = crate::common::put_json_with_token(
        &app,
        &path,
        &serde_json::json!({ "project_ids": [PROJECT_ID] }),
        &manager,
    )
    .await;
    assert_eq!(s, 403, "manager cannot set grants");
}
