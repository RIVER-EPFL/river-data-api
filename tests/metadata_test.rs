//! E2E tests for metadata endpoints: projects and sites.
//!
//! Run with: cargo test --test metadata_test
//! Requires: DATABASE_URL pointing to a TimescaleDB instance.

mod common;

use serial_test::serial;

// ============================================================================
// Helper: setup, cleanup, seed, and build app
// ============================================================================

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router) {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;
    let app = common::build_test_app(db.clone());
    (db, app)
}

// =============================================================================
// Projects: GET /api/private/projects
// =============================================================================

#[tokio::test]
#[serial]
async fn test_list_projects() {
    let (_db, app) = setup().await;

    let (status, body) = common::get_json(&app, "/api/private/projects").await;
    assert_eq!(status, 200);

    let projects = body.as_array().expect("response should be an array");
    assert_eq!(projects.len(), 1);

    let p = &projects[0];
    assert_eq!(p["id"], common::PROJECT_ID);
    assert_eq!(p["name"], "Test River Project");
    assert_eq!(p["description"], "E2E test project");
}

// =============================================================================
// Projects: GET /api/private/projects/{id} — by UUID
// =============================================================================

#[tokio::test]
#[serial]
async fn test_get_project_by_uuid() {
    let (_db, app) = setup().await;

    let uri = format!("/api/private/projects/{}", common::PROJECT_ID);
    let (status, body) = common::get_json(&app, &uri).await;
    assert_eq!(status, 200);

    assert_eq!(body["id"], common::PROJECT_ID);
    assert_eq!(body["name"], "Test River Project");
    assert_eq!(body["description"], "E2E test project");
}

// =============================================================================
// Projects: GET /api/private/projects/{name} — case-insensitive name lookup
// =============================================================================

#[tokio::test]
#[serial]
async fn test_get_project_by_name() {
    let (_db, app) = setup().await;

    // Exact case
    let (status, body) =
        common::get_json(&app, "/api/private/projects/Test%20River%20Project").await;
    assert_eq!(status, 200);
    assert_eq!(body["id"], common::PROJECT_ID);

    // Different case — proves LOWER() lookup works
    let (status, body) =
        common::get_json(&app, "/api/private/projects/TEST%20RIVER%20PROJECT").await;
    assert_eq!(status, 200);
    assert_eq!(body["id"], common::PROJECT_ID);
}

// =============================================================================
// Projects: GET /api/private/projects/{id}/sites
// =============================================================================

#[tokio::test]
#[serial]
async fn test_list_project_sites() {
    let (_db, app) = setup().await;

    let uri = format!("/api/private/projects/{}/sites", common::PROJECT_ID);
    let (status, body) = common::get_json(&app, &uri).await;
    assert_eq!(status, 200);

    let sites = body.as_array().expect("response should be an array");
    assert_eq!(sites.len(), 2);

    // Ordered by name: "Downstream Station" before "Upstream Station"
    assert_eq!(sites[0]["name"], "Downstream Station");
    assert_eq!(sites[0]["id"], common::SITE2_ID);
    assert_eq!(sites[0]["project_id"], common::PROJECT_ID);

    assert_eq!(sites[1]["name"], "Upstream Station");
    assert_eq!(sites[1]["id"], common::SITE1_ID);

    // Verify response shape includes coordinate fields
    for site in sites {
        assert!(site.get("latitude").is_some());
        assert!(site.get("longitude").is_some());
        assert!(site.get("altitude_m").is_some());
    }
}

// =============================================================================
// Projects: GET /api/private/projects/{bad_uuid} — 404
// =============================================================================

#[tokio::test]
#[serial]
async fn test_get_project_not_found() {
    let (_db, app) = setup().await;

    let (status, body) = common::get_json(
        &app,
        "/api/private/projects/00000000-0000-0000-0000-ffffffffffff",
    )
    .await;
    assert_eq!(status, 404);
    assert!(body["error"].as_str().is_some(), "should have error field");
}

// =============================================================================
// Sites: GET /api/private/sites
// =============================================================================

#[tokio::test]
#[serial]
async fn test_list_sites() {
    let (_db, app) = setup().await;

    let (status, body) = common::get_json(&app, "/api/private/sites").await;
    assert_eq!(status, 200);

    let sites = body.as_array().expect("response should be an array");
    assert_eq!(sites.len(), 2);

    // Ordered by name
    assert_eq!(sites[0]["name"], "Downstream Station");
    assert_eq!(sites[1]["name"], "Upstream Station");
}

// =============================================================================
// Sites: GET /api/private/sites/{id} — enriched detail
// =============================================================================

#[tokio::test]
#[serial]
async fn test_get_site_detail() {
    let (_db, app) = setup().await;

    // --- Site 1: full enriched response ---
    let uri = format!("/api/private/sites/{}", common::SITE1_ID);
    let (status, body) = common::get_json(&app, &uri).await;
    assert_eq!(status, 200);

    assert_eq!(body["id"], common::SITE1_ID);
    assert_eq!(body["name"], "Upstream Station");
    assert!(body["latitude"].is_number());
    assert!(body["longitude"].is_number());

    // Embedded project reference
    assert_eq!(body["project"]["id"], common::PROJECT_ID);
    assert_eq!(body["project"]["name"], "Test River Project");

    // Parameters: site 1 has 5 active parameters
    let params = body["parameters"]
        .as_array()
        .expect("parameters should be an array");
    assert_eq!(params.len(), 5);

    // Verify parameter fields
    for param in params {
        assert!(param["id"].is_string());
        assert!(param["name"].is_string());
        assert!(param["sensor_type"].is_string());
        assert!(param.get("display_units").is_some());
        assert!(param.get("is_active").is_some());
    }

    // Parameters ordered by name
    let names: Vec<&str> = params.iter().map(|p| p["name"].as_str().unwrap()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "parameters should be ordered by name");

    // Data range populated from readings
    assert!(!body["data_start"].is_null());
    assert!(!body["data_end"].is_null());
    assert_eq!(
        body["reading_count"].as_i64().unwrap(),
        1440,
        "site 1: 5 params * 288 readings"
    );

    // --- Site 2: verify different param count ---
    let uri = format!("/api/private/sites/{}", common::SITE2_ID);
    let (status, body) = common::get_json(&app, &uri).await;
    assert_eq!(status, 200);
    assert_eq!(body["parameters"].as_array().unwrap().len(), 4);
    assert_eq!(
        body["reading_count"].as_i64().unwrap(),
        1152,
        "site 2: 4 params * 288 readings"
    );
}

// =============================================================================
// Sites: GET /api/private/sites/{name} — case-insensitive name lookup
// =============================================================================

#[tokio::test]
#[serial]
async fn test_get_site_by_name() {
    let (_db, app) = setup().await;

    // Exact case
    let (status, body) = common::get_json(&app, "/api/private/sites/Upstream%20Station").await;
    assert_eq!(status, 200);
    assert_eq!(body["id"], common::SITE1_ID);

    // Different case
    let (status, body) = common::get_json(&app, "/api/private/sites/downstream%20station").await;
    assert_eq!(status, 200);
    assert_eq!(body["id"], common::SITE2_ID);
}

// =============================================================================
// Sites: GET /api/private/sites/{id}/parameters
// =============================================================================

#[tokio::test]
#[serial]
async fn test_list_site_parameters() {
    let (_db, app) = setup().await;

    let uri = format!("/api/private/sites/{}/parameters", common::SITE1_ID);
    let (status, body) = common::get_json(&app, &uri).await;
    assert_eq!(status, 200);

    let params = body.as_array().expect("response should be an array");
    assert_eq!(params.len(), 5, "site 1 has 5 active parameters");

    // Verify ordered by name
    let names: Vec<&str> = params.iter().map(|p| p["name"].as_str().unwrap()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);

    // All should be active
    for param in params {
        assert_eq!(param["is_active"], true);
    }
}

// =============================================================================
// Sites: GET /api/private/sites/{bad_uuid} — 404
// =============================================================================

#[tokio::test]
#[serial]
async fn test_get_site_not_found() {
    let (_db, app) = setup().await;

    let (status, body) = common::get_json(
        &app,
        "/api/private/sites/00000000-0000-0000-0000-ffffffffffff",
    )
    .await;
    assert_eq!(status, 404);
    assert!(body["error"].as_str().is_some(), "should have error field");
}
