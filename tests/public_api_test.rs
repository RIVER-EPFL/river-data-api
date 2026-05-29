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

/// Set up a public project with exposed parameters for public API testing.
async fn setup_public() -> (sea_orm::DatabaseConnection, axum::Router) {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;

    // Make the test project public with a slug
    exec(
        &db,
        &format!(
            "UPDATE projects SET is_public = true, public_slug = 'test-river' WHERE id = '{}'",
            common::PROJECT_ID
        ),
    )
    .await;

    // Give site 1 a public slug
    exec(
        &db,
        &format!(
            "UPDATE sites SET public_slug = 'upstream' WHERE id = '{}'",
            common::SITE1_ID,
        ),
    )
    .await;

    // Expose the Temperature parameter publicly
    exec(
        &db,
        &format!(
            "INSERT INTO public_exposed_parameters (id, project_id, parameter_id, public_name, public_units, sort_order, conversion_factor, conversion_offset, include_derived) \
             VALUES (gen_random_uuid(), '{}', '{}', 'Temperature', '°C', 1, 1.0, 0.0, false)",
            common::PROJECT_ID,
            common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await;

    let app = common::build_test_app(db.clone());
    (db, app)
}

// ============================================================================
// Nonexistent slug → 404
// ============================================================================

#[tokio::test]
#[serial]
async fn test_nonexistent_slug_returns_404() {
    let (_db, app) = setup_public().await;

    let (status, _body) = common::get(&app, "/api/public/nonexistent-slug/sites").await;
    assert_eq!(status, 404, "nonexistent project slug should return 404");
}

// ============================================================================
// Non-public project → 404
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
    // Only site 1 has a public_slug, but the endpoint may return all sites
    assert!(!sites.is_empty(), "should have at least one site");
}

// ============================================================================
// Public readings
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

    // Should have Temperature parameter exposed
    let params = body["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 1, "should have 1 exposed parameter");
    assert_eq!(params[0]["name"].as_str().unwrap(), "Temperature");
    assert_eq!(params[0]["units"].as_str().unwrap(), "°C");

    let times = body["times"].as_array().unwrap();
    assert!(!times.is_empty(), "should have readings");
}

// ============================================================================
// Unit conversion with factor=0 (offset-only)
// ============================================================================

#[tokio::test]
#[serial]
async fn test_public_readings_zero_conversion_factor() {
    let (db, app) = setup_public().await;

    exec(
        &db,
        &format!(
            "UPDATE public_exposed_parameters SET conversion_factor = 0, conversion_offset = 100 \
             WHERE project_id = '{}' AND parameter_id = '{}'",
            common::PROJECT_ID,
            common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await;

    let (status, body) = common::get_json(
        &app,
        "/api/public/test-river/sites/upstream/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T01:00:00Z",
    )
    .await;

    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 1);

    let values = params[0]["values"].as_array().unwrap();
    for (i, v) in values.iter().enumerate() {
        if let Some(val) = v.as_f64() {
            assert!(
                (val - 100.0).abs() < 1e-10,
                "with factor=0 and offset=100, all values should be 100.0, got {val} at index {i}"
            );
        }
    }
}

// ============================================================================
// Multi-exposure: same parameter exposed under two names (DOuM + DOmgL)
// ============================================================================

async fn setup_multi_exposure() -> (sea_orm::DatabaseConnection, axum::Router) {
    let (db, app) = setup_public().await;

    // Expose Dissolved_O2 as DOuM (identity)
    exec(
        &db,
        &format!(
            "INSERT INTO public_exposed_parameters (id, project_id, parameter_id, public_name, public_units, sort_order, conversion_factor, conversion_offset, include_derived) \
             VALUES (gen_random_uuid(), '{}', '{}', 'DOuM', 'µM', 2, 1.0, 0.0, false)",
            common::PROJECT_ID,
            common::GLOBAL_PARAM_DO_ID,
        ),
    )
    .await;

    // Expose Dissolved_O2 again as DOmgL (factor=0.032)
    exec(
        &db,
        &format!(
            "INSERT INTO public_exposed_parameters (id, project_id, parameter_id, public_name, public_units, sort_order, conversion_factor, conversion_offset, include_derived) \
             VALUES (gen_random_uuid(), '{}', '{}', 'DOmgL', 'mg/L', 3, 0.032, 0.0, false)",
            common::PROJECT_ID,
            common::GLOBAL_PARAM_DO_ID,
        ),
    )
    .await;

    (db, app)
}

#[tokio::test]
#[serial]
async fn test_multi_exposure_same_parameter() {
    let (_db, app) = setup_multi_exposure().await;

    let (status, body) = common::get_json(
        &app,
        "/api/public/test-river/sites/upstream/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T01:00:00Z",
    )
    .await;
    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 3, "should have Temperature, DOuM, DOmgL");

    let doum = params.iter().find(|p| p["name"] == "DOuM").expect("DOuM missing");
    let domgl = params.iter().find(|p| p["name"] == "DOmgL").expect("DOmgL missing");

    assert_eq!(doum["units"], "µM");
    assert_eq!(domgl["units"], "mg/L");

    let doum_values = doum["values"].as_array().unwrap();
    let domgl_values = domgl["values"].as_array().unwrap();
    assert_eq!(doum_values.len(), domgl_values.len());

    for (i, (u, m)) in doum_values.iter().zip(domgl_values.iter()).enumerate() {
        if let (Some(uv), Some(mv)) = (u.as_f64(), m.as_f64()) {
            let expected = uv * 0.032;
            assert!(
                (mv - expected).abs() < 1e-6,
                "DOmgL[{i}] = {mv} != DOuM[{i}] * 0.032 = {expected}"
            );
        }
    }
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
    assert_eq!(projects[0]["slug"], "test-river");
    assert!(projects[0]["docs_url"].as_str().unwrap().contains("/docs"));
    assert!(projects[0]["sites_url"].as_str().unwrap().contains("/sites"));
}

// ============================================================================
// Aggregates with conversion
// ============================================================================

#[tokio::test]
#[serial]
async fn test_public_aggregates_with_conversion() {
    let (_db, app) = setup_multi_exposure().await;

    let (status, body) = common::get_json(
        &app,
        "/api/public/test-river/sites/upstream/aggregates/hourly?start=2025-01-15T00:00:00Z&end=2025-01-15T12:00:00Z",
    )
    .await;
    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 3, "aggregates should have Temperature, DOuM, DOmgL");

    let doum = params.iter().find(|p| p["name"] == "DOuM").expect("DOuM aggregates missing");
    let domgl = params.iter().find(|p| p["name"] == "DOmgL").expect("DOmgL aggregates missing");

    let doum_avg = doum["avg"].as_array().unwrap();
    let domgl_avg = domgl["avg"].as_array().unwrap();

    for (i, (u, m)) in doum_avg.iter().zip(domgl_avg.iter()).enumerate() {
        if let (Some(uv), Some(mv)) = (u.as_f64(), m.as_f64()) {
            let expected = uv * 0.032;
            assert!(
                (mv - expected).abs() < 1e-6,
                "DOmgL avg[{i}] = {mv} != DOuM avg[{i}] * 0.032 = {expected}"
            );
        }
    }
}

// ============================================================================
// CSV format
// ============================================================================

#[tokio::test]
#[serial]
async fn test_public_readings_csv() {
    let (_db, app) = setup_multi_exposure().await;

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
    assert!(header.contains("DOuM"), "CSV header should include DOuM");
    assert!(header.contains("DOmgL"), "CSV header should include DOmgL");
    assert!(header.contains("Temperature"), "CSV header should include Temperature");

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
    let (_db, app) = setup_multi_exposure().await;

    let (status, body) = common::get_json(
        &app,
        "/api/public/test-river/sites/upstream/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T01:00:00Z&parameters=DOmgL",
    )
    .await;
    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 1, "should only have DOmgL");
    assert_eq!(params[0]["name"], "DOmgL");
    assert_eq!(params[0]["units"], "mg/L");
}

// ============================================================================
// Site detail with data range
// ============================================================================

#[tokio::test]
#[serial]
async fn test_public_site_detail() {
    let (_db, app) = setup_multi_exposure().await;

    let (status, body) = common::get_json(
        &app,
        "/api/public/test-river/sites/upstream",
    )
    .await;
    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 3, "site detail should list all 3 exposed params");

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
    let (_db, app) = setup_multi_exposure().await;

    let (status, body) = common::get_json(
        &app,
        "/api/public/test-river/sites/upstream/parameters",
    )
    .await;
    assert_eq!(status, 200);

    let params = body.as_array().unwrap();
    assert_eq!(params.len(), 3);

    let names: Vec<&str> = params.iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"Temperature"));
    assert!(names.contains(&"DOuM"));
    assert!(names.contains(&"DOmgL"));
}
