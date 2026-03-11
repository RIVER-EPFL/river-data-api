//! Smoke test: validates that the test infrastructure works end-to-end.
//!
//! Run with: cargo test --test smoke_test
//! Requires: DATABASE_URL pointing to a TimescaleDB instance.

mod common;

use serial_test::serial;

#[tokio::test]
#[serial]
async fn test_infra_seed_and_healthz() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;

    let app = common::build_test_app(db.clone());

    // Health check should always return 200
    let (status, _body) = common::get(&app, "/healthz").await;
    assert_eq!(status, 200, "healthz should return 200");

    // Verify seed data: should have 1 project
    let (status, json) = common::get_json(&app, "/api/private/projects").await;
    assert_eq!(status, 200, "projects endpoint should return 200");
    let projects = json.as_array().expect("projects should be an array");
    assert!(!projects.is_empty(), "should have at least 1 project");

    common::cleanup_test_db(&db).await;
}
