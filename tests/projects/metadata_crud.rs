//! E2E tests for metadata endpoints: projects and sites.
//!
//! Run with: cargo test --test projects
//! Requires: DATABASE_URL pointing to a TimescaleDB instance.


use serial_test::serial;

// ============================================================================
// Helper: setup, cleanup, seed, and build app
// ============================================================================

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

// =============================================================================
// Projects: GET /api/projects — CrudCrate list
// =============================================================================

#[tokio::test]
#[serial]
async fn test_list_projects() {
    let (_db, app, token) = setup().await;

    let (status, body) = crate::common::get_json_with_token(&app, "/api/projects", &token).await;
    assert_eq!(status, 200);

    let projects = body.as_array().expect("response should be an array");
    assert_eq!(projects.len(), 1);

    let p = &projects[0];
    assert_eq!(p["id"], crate::common::PROJECT_ID);
    assert_eq!(p["name"], "Test River Project");
    assert_eq!(p["description"], "E2E test project");
}

// =============================================================================
// Projects: GET /api/projects/{id} — CrudCrate get by UUID
// =============================================================================

#[tokio::test]
#[serial]
async fn test_get_project_by_uuid() {
    let (_db, app, token) = setup().await;

    let uri = format!("/api/projects/{}", crate::common::PROJECT_ID);
    let (status, body) = crate::common::get_json_with_token(&app, &uri, &token).await;
    assert_eq!(status, 200);

    assert_eq!(body["id"], crate::common::PROJECT_ID);
    assert_eq!(body["name"], "Test River Project");
    assert_eq!(body["description"], "E2E test project");
}

// =============================================================================
// Projects: GET /api/projects/{id}/sites
// =============================================================================

#[tokio::test]
#[serial]
async fn test_list_project_sites() {
    let (_db, app, token) = setup().await;

    let uri = format!("/api/projects/{}/sites", crate::common::PROJECT_ID);
    let (status, body) = crate::common::get_json_with_token(&app, &uri, &token).await;
    assert_eq!(status, 200);

    let sites = body.as_array().expect("response should be an array");
    assert_eq!(sites.len(), 2);

    // Ordered by name: "Downstream Station" before "Upstream Station"
    assert_eq!(sites[0]["name"], "Downstream Station");
    assert_eq!(sites[0]["id"], crate::common::SITE2_ID);
    assert_eq!(sites[0]["project_id"], crate::common::PROJECT_ID);

    assert_eq!(sites[1]["name"], "Upstream Station");
    assert_eq!(sites[1]["id"], crate::common::SITE1_ID);

    // Verify response shape includes coordinate fields
    for site in sites {
        assert!(site.get("latitude").is_some());
        assert!(site.get("longitude").is_some());
        assert!(site.get("altitude_m").is_some());
    }
}

// =============================================================================
// Projects: GET /api/projects/{bad_uuid} — 404
// =============================================================================

#[tokio::test]
#[serial]
async fn test_get_project_not_found() {
    let (_db, app, token) = setup().await;

    let (status, _body) = crate::common::get_with_token(
        &app,
        "/api/projects/00000000-0000-0000-0000-ffffffffffff",
        &token,
    )
    .await;
    assert_eq!(status, 404);
}

// =============================================================================
// Sites: GET /api/sites — CrudCrate list
// =============================================================================

#[tokio::test]
#[serial]
async fn test_list_sites() {
    let (_db, app, token) = setup().await;

    let (status, body) = crate::common::get_json_with_token(&app, "/api/sites", &token).await;
    assert_eq!(status, 200);

    let sites = body.as_array().expect("response should be an array");
    assert_eq!(sites.len(), 2);
}

// =============================================================================
// Sites: GET /api/sites/{id} — CrudCrate get by UUID
// =============================================================================

#[tokio::test]
#[serial]
async fn test_get_site_by_uuid() {
    let (_db, app, token) = setup().await;

    let uri = format!("/api/sites/{}", crate::common::SITE1_ID);
    let (status, body) = crate::common::get_json_with_token(&app, &uri, &token).await;
    assert_eq!(status, 200);

    assert_eq!(body["id"], crate::common::SITE1_ID);
    assert_eq!(body["name"], "Upstream Station");
    assert!(body["latitude"].is_number());
    assert!(body["longitude"].is_number());
}

// =============================================================================
// Sites: GET /api/sites/{id}/parameters
// =============================================================================

#[tokio::test]
#[serial]
async fn test_list_site_parameters() {
    let (_db, app, token) = setup().await;

    let uri = format!("/api/sites/{}/parameters", crate::common::SITE1_ID);
    let (status, body) = crate::common::get_json_with_token(&app, &uri, &token).await;
    assert_eq!(status, 200);

    let params = body.as_array().expect("response should be an array");
    assert_eq!(params.len(), 5, "site 1 has 5 active parameters");

    // Verify ordered by code
    let codes: Vec<&str> = params.iter().map(|p| p["code"].as_str().unwrap()).collect();
    let mut sorted = codes.clone();
    sorted.sort();
    assert_eq!(codes, sorted);

    // All should be active
    for param in params {
        assert_eq!(param["is_active"], true);
    }
}

// =============================================================================
// Sites: GET /api/sites/{bad_uuid} — 404
// =============================================================================

#[tokio::test]
#[serial]
async fn test_get_site_not_found() {
    let (_db, app, token) = setup().await;

    let (status, _body) = crate::common::get_with_token(
        &app,
        "/api/sites/00000000-0000-0000-0000-ffffffffffff",
        &token,
    )
    .await;
    assert_eq!(status, 404);
}
