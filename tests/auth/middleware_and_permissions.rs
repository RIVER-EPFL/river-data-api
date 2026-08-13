//! Auth middleware tests: verify token validation and permission enforcement.
//!
//! Run with: cargo test --test auth
//! Requires: DATABASE_URL pointing to a TimescaleDB instance.

use chrono::{Duration, Utc};
use serial_test::serial;

// ============================================================================
// Helper
// ============================================================================

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app)
}

// ============================================================================
// Unauthenticated access
// ============================================================================

#[tokio::test]
#[serial]
async fn test_unauthenticated_get_returns_401() {
    let (_db, app) = setup().await;

    // No Authorization header at all
    let (status, _body) = crate::common::get(&app, "/api/projects").await;
    assert_eq!(
        status, 401,
        "unauthenticated GET to service tier should return 401"
    );
}

// ============================================================================
// Expired token
// ============================================================================

#[tokio::test]
#[serial]
async fn test_expired_token_returns_401() {
    let (db, app) = setup().await;

    let expired_token = crate::common::seed_api_token_with_expiry(
        &db,
        crate::common::full_permissions(),
        None,
        Utc::now() - Duration::hours(1),
    )
    .await;

    let (status, _body) =
        crate::common::get_with_token(&app, "/api/projects", &expired_token).await;
    assert_eq!(status, 401, "expired token should return 401");
}

// ============================================================================
// Inactive token
// ============================================================================

#[tokio::test]
#[serial]
async fn test_inactive_token_returns_401() {
    let (db, app) = setup().await;

    let inactive_token =
        crate::common::seed_inactive_api_token(&db, crate::common::full_permissions()).await;

    let (status, _body) =
        crate::common::get_with_token(&app, "/api/projects", &inactive_token).await;
    assert_eq!(status, 401, "inactive token should return 401");
}

// ============================================================================
// Permission enforcement: read_metadata vs read_data
// ============================================================================

#[tokio::test]
#[serial]
async fn test_read_metadata_only_token() {
    let (db, app) = setup().await;

    let token = crate::common::seed_api_token(
        &db,
        serde_json::json!({
            "read_metadata": true,
            "read_data": false,
            "write_metadata": false,
            "write_data": false,
        }),
        None,
    )
    .await;

    // GET projects (read_metadata) → 200
    let (status, _body) = crate::common::get_with_token(&app, "/api/projects", &token).await;
    assert_eq!(status, 200, "read_metadata token should access projects");

    // GET readings (read_data) → 403
    let site_id = crate::common::SITE1_ID;
    let (status, _body) = crate::common::get_with_token(
        &app,
        &format!(
            "/api/sites/{site_id}/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T12:00:00Z"
        ),
        &token,
    )
    .await;
    assert_eq!(
        status, 403,
        "read_metadata-only token should be denied readings access"
    );
}

// ============================================================================
// Project scope enforcement
// ============================================================================

#[tokio::test]
#[serial]
async fn test_project_scoped_token_cannot_access_other_project_site() {
    let (db, app) = setup().await;

    // Create a second project
    let other_project_id = "00000000-0000-4000-a000-000000000099";
    use sea_orm::{ConnectionTrait, Statement};
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "INSERT INTO projects (id, name, description) VALUES ('{other_project_id}', 'Other Project', 'Another project')"
        ),
    ))
    .await
    .unwrap();

    // Token scoped to the other project
    let token = crate::common::seed_api_token(
        &db,
        crate::common::full_permissions(),
        Some(other_project_id),
    )
    .await;

    // Try to access site 1 (belongs to PROJECT_ID, not other_project_id)
    let site_id = crate::common::SITE1_ID;
    let (status, _body) = crate::common::get_with_token(
        &app,
        &format!(
            "/api/sites/{site_id}/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T12:00:00Z"
        ),
        &token,
    )
    .await;
    assert_eq!(
        status, 403,
        "project-scoped token should be denied access to other project's sites"
    );
}

// ============================================================================
// Malformed Authorization header
// ============================================================================

#[tokio::test]
#[serial]
async fn test_malformed_auth_header_returns_401() {
    let (_db, app) = setup().await;

    // "NotBearer xyz" instead of "Bearer xyz"
    let (status, _body) =
        crate::common::get_with_auth_header(&app, "/api/projects", "NotBearer xyz").await;
    assert_eq!(
        status, 401,
        "malformed auth header should return 401, not 500"
    );
}

// ============================================================================
// Empty token after Bearer prefix
// ============================================================================

#[tokio::test]
#[serial]
async fn test_empty_bearer_token_returns_401() {
    let (_db, app) = setup().await;

    let (status, _body) =
        crate::common::get_with_auth_header(&app, "/api/projects", "Bearer ").await;
    assert_eq!(status, 401, "empty bearer token should return 401");
}
