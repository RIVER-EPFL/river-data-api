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

    // Update the exposed parameter to have factor=0, offset=100
    // This means all values become 0*raw + 100 = 100 (constant)
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

    // All values should be 100.0 (factor=0 means raw*0=0, plus offset=100)
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
