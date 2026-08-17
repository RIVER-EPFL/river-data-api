//! Anti-backdoor: a linked chat's authority is the linked Keycloak user's *current* state, resolved
//! live. A user who is gone, disabled, or stripped of riverdata roles resolves to `Revoked` (which
//! deactivates the link); an unreachable Keycloak fails closed (`None`) without deactivating.
//!
//! A tiny in-process mock stands in for Keycloak's admin API, keyed off the sub so responses are
//! deterministic:
//!   admin-user → enabled + riverdata-admin    regular-user → enabled + riverdata-river
//!   no-role-user → enabled, no roles           disabled-user → enabled=false    gone-user → 404
//!
//! Run: cargo test --test notifications -- --test-threads=1

use axum::extract::Path;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use river_db::common::AppState;
use river_db::common::authz::Role;
use river_db::routes::private::notifications::authz::RoleResolution;
use river_db::routes::private::notifications::reconcile;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

async fn token() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "access_token": "mock-token", "expires_in": 300 }))
}

async fn user(Path(sub): Path<String>) -> axum::response::Response {
    match sub.as_str() {
        "gone-user" => axum::http::StatusCode::NOT_FOUND.into_response(),
        "disabled-user" => Json(serde_json::json!({ "enabled": false })).into_response(),
        _ => Json(serde_json::json!({ "enabled": true })).into_response(),
    }
}

async fn roles(Path(sub): Path<String>) -> Json<serde_json::Value> {
    let names = match sub.as_str() {
        "admin-user" => vec!["riverdata-admin"],
        "regular-user" => vec!["riverdata-river"],
        _ => vec![],
    };
    Json(serde_json::json!(
        names
            .into_iter()
            .map(|n| serde_json::json!({ "name": n }))
            .collect::<Vec<_>>()
    ))
}

/// Spawn the mock and return its base URL.
async fn spawn_mock_keycloak() -> String {
    let app = Router::new()
        .route("/realms/mock/protocol/openid-connect/token", post(token))
        .route("/admin/realms/mock/users/{sub}", get(user))
        .route(
            "/admin/realms/mock/users/{sub}/role-mappings/realm",
            get(roles),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn state_with_keycloak(db: DatabaseConnection, base: &str) -> AppState {
    let config = crate::common::keycloak::test_config_with_mock_keycloak(base);
    AppState::new(db, config, None)
}

async fn insert_identity(db: &DatabaseConnection, sub: &str, chat_id: i64) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO telegram_identities (linked_keycloak_sub, telegram_chat_id, is_active) \
             VALUES ('{sub}', {chat_id}, TRUE)"
        ),
    )
    .await;
}

async fn is_active(db: &DatabaseConnection, sub: &str) -> bool {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT is_active FROM telegram_identities WHERE linked_keycloak_sub = '{sub}'"),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<bool>("", "is_active")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn resolves_current_role_from_keycloak() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let base = spawn_mock_keycloak().await;
    let state = state_with_keycloak(db.clone(), &base);

    assert_eq!(
        state.authorizer.resolve(&state, "admin-user").await,
        Some(RoleResolution::Active(Role::Administrator))
    );
    assert_eq!(
        state.authorizer.resolve(&state, "regular-user").await,
        Some(RoleResolution::Active(Role::River))
    );
    assert_eq!(
        state.authorizer.resolve(&state, "no-role-user").await,
        Some(RoleResolution::Revoked),
        "a user with no riverdata role is revoked"
    );
    assert_eq!(
        state.authorizer.resolve(&state, "disabled-user").await,
        Some(RoleResolution::Revoked),
        "a disabled user is revoked"
    );
    assert_eq!(
        state.authorizer.resolve(&state, "gone-user").await,
        Some(RoleResolution::Revoked),
        "a deleted user is revoked"
    );
}

#[tokio::test]
#[serial]
async fn unavailable_keycloak_fails_closed() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    // Nothing listening here, connection refused.
    let state = state_with_keycloak(db.clone(), "http://127.0.0.1:1");

    assert_eq!(
        state.authorizer.resolve(&state, "regular-user").await,
        None,
        "an unreachable Keycloak denies (None), it never assumes a role"
    );
}

#[tokio::test]
#[serial]
async fn reconcile_deactivates_revoked_links_only() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let base = spawn_mock_keycloak().await;
    let state = state_with_keycloak(db.clone(), &base);

    insert_identity(&db, "regular-user", 1).await;
    insert_identity(&db, "no-role-user", 2).await;
    insert_identity(&db, "disabled-user", 3).await;

    let deactivated = reconcile::sweep(&state).await.unwrap().revoked;
    assert_eq!(
        deactivated, 2,
        "the no-role and disabled users are deactivated"
    );

    assert!(
        is_active(&db, "regular-user").await,
        "an active user keeps the link"
    );
    assert!(!is_active(&db, "no-role-user").await);
    assert!(!is_active(&db, "disabled-user").await);
}

#[tokio::test]
#[serial]
async fn reconcile_keeps_links_when_keycloak_unavailable() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let state = state_with_keycloak(db.clone(), "http://127.0.0.1:1");

    insert_identity(&db, "regular-user", 1).await;
    let deactivated = reconcile::sweep(&state).await.unwrap().revoked;
    assert_eq!(deactivated, 0, "an outage must not mass-deactivate");
    assert!(
        is_active(&db, "regular-user").await,
        "link survives the outage"
    );
}

/// `expiry_exempt` holds a link open against *inactivity*. It must never shield a user whose
/// Keycloak account is gone: the revocation pass runs first and unconditionally.
#[tokio::test]
#[serial]
async fn a_pinned_link_is_still_revoked() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let base = spawn_mock_keycloak().await;
    let state = state_with_keycloak(db.clone(), &base);

    crate::common::exec(
        &db,
        "INSERT INTO telegram_identities \
         (linked_keycloak_sub, telegram_chat_id, is_active, expiry_exempt, last_verified_at) \
         VALUES ('disabled-user', 4242, TRUE, TRUE, NOW())",
    )
    .await;

    let outcome = reconcile::sweep(&state).await.unwrap();
    assert_eq!(outcome.revoked, 1, "a pinned link is not exempt from revocation");
    assert!(
        !is_active(&db, "disabled-user").await,
        "a revoked user's link is deactivated even when pinned against idle expiry"
    );
}
