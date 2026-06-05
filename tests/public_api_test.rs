//! Tests for the public API tier (/api/public/).
//!
//! Run with: cargo test --test public_api_test
//! Requires: DATABASE_URL pointing to a TimescaleDB instance.

mod common;

use serial_test::serial;

// ============================================================================
// Helper
// ============================================================================

async fn exec(db: &sea_orm::DatabaseConnection, sql: &str) {
    use sea_orm::{ConnectionTrait, Statement};
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("SQL failed: {e}\nQuery: {sql}"));
}

/// Set up a public project with one exposed parameter (DO_Temperature) for public API testing.
async fn setup_public() -> (sea_orm::DatabaseConnection, axum::Router) {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;

    exec(
        &db,
        &format!(
            "UPDATE projects SET is_public = true, public_code = 'test-river' WHERE id = '{}'",
            common::PROJECT_ID
        ),
    )
    .await;

    exec(
        &db,
        &format!(
            "UPDATE sites SET public_code = 'upstream' WHERE id = '{}'",
            common::SITE1_ID,
        ),
    )
    .await;

    // Expose the DO_Temperature site_parameter publicly
    exec(
        &db,
        &format!(
            "UPDATE site_parameters SET is_public = true WHERE id = '{}'",
            common::PARAM_S1_TEMP_ID,
        ),
    )
    .await;

    let app = common::build_test_app(db.clone());
    (db, app)
}

/// Set up a public project with two exposed parameters (DO_Temperature + Dissolved_O2).
async fn setup_two_params() -> (sea_orm::DatabaseConnection, axum::Router) {
    let (db, app) = setup_public().await;

    // Also expose Dissolved_O2 at site 1
    exec(
        &db,
        &format!(
            "UPDATE site_parameters SET is_public = true WHERE id = '{}'",
            common::PARAM_S1_DO_ID,
        ),
    )
    .await;

    (db, app)
}

// ============================================================================
// Nonexistent code -> 404
// ============================================================================

#[tokio::test]
#[serial]
async fn test_nonexistent_code_returns_404() {
    let (_db, app) = setup_public().await;

    let (status, _body) = common::get(&app, "/api/public/nonexistent-code/sites").await;
    assert_eq!(status, 404, "nonexistent project code should return 404");
}

// ============================================================================
// Non-public project -> 404
// ============================================================================

#[tokio::test]
#[serial]
async fn test_non_public_project_returns_404() {
    let (db, app) = setup_public().await;

    // Make the project non-public
    exec(
        &db,
        &format!(
            "UPDATE projects SET is_public = false WHERE id = '{}'",
            common::PROJECT_ID,
        ),
    )
    .await;

    let (status, _body) = common::get(&app, "/api/public/test-river/sites").await;
    assert_eq!(status, 404, "non-public project should return 404");
}

// ============================================================================
// Public site listing
// ============================================================================

#[tokio::test]
#[serial]
async fn test_public_list_sites() {
    let (_db, app) = setup_public().await;

    let (status, body) = common::get_json(&app, "/api/public/test-river/sites").await;
    assert_eq!(status, 200);

    let sites = body.as_array().expect("response should be an array");
    assert!(!sites.is_empty(), "should have at least one site");
}

// ============================================================================
// Public readings (single parameter)
// ============================================================================

#[tokio::test]
#[serial]
async fn test_public_readings() {
    let (_db, app) = setup_public().await;

    let (status, body) = common::get_json(
        &app,
        "/api/public/test-river/sites/upstream/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T12:00:00Z",
    )
    .await;

    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 1, "should have 1 exposed parameter");
    assert_eq!(params[0]["code"].as_str().unwrap(), "DO_Temperature");
    assert_eq!(params[0]["units"].as_str().unwrap(), "°C");

    let times = body["times"].as_array().unwrap();
    assert!(!times.is_empty(), "should have readings");
}

// ============================================================================
// Public readings with two parameters
// ============================================================================

#[tokio::test]
#[serial]
async fn test_public_readings_two_params() {
    let (_db, app) = setup_two_params().await;

    let (status, body) = common::get_json(
        &app,
        "/api/public/test-river/sites/upstream/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T01:00:00Z",
    )
    .await;
    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 2, "should have DO_Temperature and Dissolved_O2");

    let temp = params.iter().find(|p| p["code"] == "DO_Temperature").expect("DO_Temperature missing");
    let do_param = params.iter().find(|p| p["code"] == "Dissolved_O2").expect("Dissolved_O2 missing");

    assert_eq!(temp["units"], "°C");
    assert_eq!(do_param["units"], "µM");

    let temp_values = temp["values"].as_array().unwrap();
    let do_values = do_param["values"].as_array().unwrap();
    assert_eq!(temp_values.len(), do_values.len());
    assert!(!temp_values.is_empty(), "should have readings");
}

// ============================================================================
// Discovery endpoint
// ============================================================================

#[tokio::test]
#[serial]
async fn test_public_discovery() {
    let (_db, app) = setup_public().await;

    let (status, body) = common::get_json(&app, "/api/public").await;
    assert_eq!(status, 200);

    let projects = body.as_array().expect("discovery should return an array");
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["code"], "test-river");
    assert!(projects[0]["docs_url"].as_str().unwrap().contains("/docs"));
    assert!(projects[0]["sites_url"].as_str().unwrap().contains("/sites"));
}

// ============================================================================
// Aggregates
// ============================================================================

#[tokio::test]
#[serial]
async fn test_public_aggregates() {
    let (_db, app) = setup_two_params().await;

    let (status, body) = common::get_json(
        &app,
        "/api/public/test-river/sites/upstream/aggregates/hourly?start=2025-01-15T00:00:00Z&end=2025-01-15T12:00:00Z",
    )
    .await;
    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 2, "aggregates should have DO_Temperature and Dissolved_O2");

    let temp = params.iter().find(|p| p["code"] == "DO_Temperature").expect("DO_Temperature aggregates missing");
    let do_param = params.iter().find(|p| p["code"] == "Dissolved_O2").expect("Dissolved_O2 aggregates missing");

    let temp_avg = temp["avg"].as_array().unwrap();
    let do_avg = do_param["avg"].as_array().unwrap();
    assert!(!temp_avg.is_empty(), "should have hourly aggregates for temperature");
    assert!(!do_avg.is_empty(), "should have hourly aggregates for dissolved oxygen");
}

// ============================================================================
// CSV format
// ============================================================================

#[tokio::test]
#[serial]
async fn test_public_readings_csv() {
    let (_db, app) = setup_two_params().await;

    let (status, body) = common::get(
        &app,
        "/api/public/test-river/sites/upstream/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T01:00:00Z&format=csv",
    )
    .await;
    assert_eq!(status, 200);

    let lines: Vec<&str> = body.lines().collect();
    assert!(!lines.is_empty(), "CSV should have at least a header");

    let header = lines[0];
    assert!(header.starts_with("time"), "CSV header should start with time");
    assert!(header.contains("DO_Temperature"), "CSV header should include DO_Temperature");
    assert!(header.contains("Dissolved_O2"), "CSV header should include Dissolved_O2");

    assert!(lines.len() > 1, "CSV should have data rows");
}

// ============================================================================
// Docs endpoint with custom title
// ============================================================================

#[tokio::test]
#[serial]
async fn test_public_docs_custom_title() {
    let (db, app) = setup_public().await;

    exec(
        &db,
        &format!(
            "UPDATE projects SET public_api_title = 'Mount Resilience Data', \
             public_api_description = 'Oxygen and temperature from alpine streams' \
             WHERE id = '{}'",
            common::PROJECT_ID,
        ),
    )
    .await;

    let (status, body) = common::get(&app, "/api/public/test-river/docs").await;
    assert_eq!(status, 200);
    assert!(body.contains("Mount Resilience Data"), "docs should include custom title");
}

// ============================================================================
// Parameter filtering
// ============================================================================

#[tokio::test]
#[serial]
async fn test_public_parameter_filtering() {
    let (_db, app) = setup_two_params().await;

    let (status, body) = common::get_json(
        &app,
        "/api/public/test-river/sites/upstream/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T01:00:00Z&parameters=Dissolved_O2",
    )
    .await;
    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 1, "should only have Dissolved_O2");
    assert_eq!(params[0]["code"], "Dissolved_O2");
    assert_eq!(params[0]["units"], "µM");
}

// ============================================================================
// Site detail with data range
// ============================================================================

#[tokio::test]
#[serial]
async fn test_public_site_detail() {
    let (_db, app) = setup_two_params().await;

    let (status, body) = common::get_json(
        &app,
        "/api/public/test-river/sites/upstream",
    )
    .await;
    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 2, "site detail should list both exposed params");

    assert!(body["reading_count"].as_i64().unwrap() > 0, "should have readings");
    assert!(body["data_start"].is_string(), "should have data_start");
    assert!(body["data_end"].is_string(), "should have data_end");
}

// ============================================================================
// List parameters endpoint
// ============================================================================

#[tokio::test]
#[serial]
async fn test_public_list_parameters() {
    let (_db, app) = setup_two_params().await;

    let (status, body) = common::get_json(
        &app,
        "/api/public/test-river/sites/upstream/parameters",
    )
    .await;
    assert_eq!(status, 200);

    let params = body.as_array().unwrap();
    assert_eq!(params.len(), 2);

    let codes: Vec<&str> = params.iter().map(|p| p["code"].as_str().unwrap()).collect();
    assert!(codes.contains(&"DO_Temperature"));
    assert!(codes.contains(&"Dissolved_O2"));
}
