//! C1, the notification project-access choke point. `accessible_project_ids` is the single seam that
//! every subscription read/write and every alert fan-out routes through: it confines a member to their
//! granted projects, leaves an administrator unrestricted, and fails closed (grants nothing) for a
//! revoked/unknown user or when Keycloak is unreachable.
//!
//! Uses the same in-process mock Keycloak as `anti_backdoor_test`:
//!   admin-user → riverdata-admin    regular-user → riverdata-river    no-role-user → no roles
//!
//! Run: cargo test --test notifications -- --test-threads=1

use axum::extract::Path;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use river_db::common::AppState;
use river_db::routes::private::notifications::access::{accessible_project_ids, project_allowed};
use sea_orm::DatabaseConnection;
use serial_test::serial;
use uuid::Uuid;

const PROJECT_A: &str = "00000000-0000-4000-a000-0000000000a1";
const PROJECT_B: &str = "00000000-0000-4000-a000-0000000000b2";

async fn token() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "access_token": "mock-token", "expires_in": 300 }))
}

async fn user(Path(_sub): Path<String>) -> axum::response::Response {
    Json(serde_json::json!({ "enabled": true })).into_response()
}

async fn roles(Path(sub): Path<String>) -> Json<serde_json::Value> {
    let names = match sub.as_str() {
        "admin-user" => vec!["riverdata-admin"],
        "regular-user" => vec!["riverdata-river"],
        _ => vec![],
    };
    Json(serde_json::json!(
        names.into_iter().map(|n| serde_json::json!({ "name": n })).collect::<Vec<_>>()
    ))
}

async fn spawn_mock_keycloak() -> String {
    let app = Router::new()
        .route("/realms/mock/protocol/openid-connect/token", post(token))
        .route("/admin/realms/mock/users/{sub}", get(user))
        .route("/admin/realms/mock/users/{sub}/role-mappings/realm", get(roles));
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

async fn seed_projects(db: &DatabaseConnection) {
    for (id, name) in [(PROJECT_A, "Project A"), (PROJECT_B, "Project B")] {
        crate::common::exec(
            db,
            &format!("INSERT INTO projects (id, name, data_source) VALUES ('{id}', '{name}', 'test')"),
        )
        .await;
    }
}

#[tokio::test]
#[serial]
async fn member_is_confined_to_granted_projects() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    seed_projects(&db).await;
    let base = spawn_mock_keycloak().await;
    let state = state_with_keycloak(db.clone(), &base);

    crate::common::keycloak::grant_project(&db, "regular-user", PROJECT_A).await;

    let accessible = accessible_project_ids(&state, "regular-user").await;
    assert!(accessible.is_some(), "a member is confined to a bounded set, not unrestricted");
    assert!(
        project_allowed(&accessible, Uuid::parse_str(PROJECT_A).unwrap()),
        "the granted project is allowed"
    );
    assert!(
        !project_allowed(&accessible, Uuid::parse_str(PROJECT_B).unwrap()),
        "an ungranted project is denied, no cross-project notification leak"
    );
}

#[tokio::test]
#[serial]
async fn administrator_is_unrestricted() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    seed_projects(&db).await;
    let base = spawn_mock_keycloak().await;
    let state = state_with_keycloak(db.clone(), &base);

    let accessible = accessible_project_ids(&state, "admin-user").await;
    assert!(accessible.is_none(), "an administrator is unrestricted");
    assert!(project_allowed(&accessible, Uuid::parse_str(PROJECT_B).unwrap()));
}

#[tokio::test]
#[serial]
async fn member_with_no_grant_gets_nothing() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    seed_projects(&db).await;
    let base = spawn_mock_keycloak().await;
    let state = state_with_keycloak(db.clone(), &base);

    // A river member who was never granted any project, the exact C1 hole. Must see nothing.
    let accessible = accessible_project_ids(&state, "regular-user").await;
    assert_eq!(accessible, Some(std::collections::HashSet::new()));
    assert!(!project_allowed(&accessible, Uuid::parse_str(PROJECT_A).unwrap()));
}

#[tokio::test]
#[serial]
async fn revoked_and_unreachable_fail_closed() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    seed_projects(&db).await;

    let base = spawn_mock_keycloak().await;
    let state = state_with_keycloak(db.clone(), &base);
    // A user holding no riverdata role resolves to Revoked → empty (fail closed).
    assert_eq!(
        accessible_project_ids(&state, "no-role-user").await,
        Some(std::collections::HashSet::new())
    );

    // Keycloak unreachable → resolution is None → still fail closed rather than over-deliver.
    let down = state_with_keycloak(db.clone(), "http://127.0.0.1:1");
    assert_eq!(
        accessible_project_ids(&down, "regular-user").await,
        Some(std::collections::HashSet::new())
    );
}
