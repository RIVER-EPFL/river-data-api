//! Smoke test: validates that the test infrastructure works end-to-end.
//!
//! Run with: cargo test --test smoke_test
//! Requires: DATABASE_URL pointing to a TimescaleDB instance.

mod common;

use sea_orm::ConnectionTrait;
use serial_test::serial;

#[tokio::test]
#[serial]
async fn test_infra_seed_and_healthz() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("river_db=debug,sea_orm=debug")
        .with_test_writer()
        .try_init();

    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;

    let app = common::build_test_app(db.clone());

    // Health check should always return 200 (no auth needed)
    let (status, _body) = common::get(&app, "/healthz").await;
    assert_eq!(status, 200, "healthz should return 200");

    // First, verify DB works directly
    let row = db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT name FROM projects WHERE id = '{}'", common::PROJECT_ID),
        ))
        .await
        .unwrap()
        .expect("Should find seeded project in DB");
    let name: String = row.try_get("", "name").unwrap();
    assert_eq!(name, "Test River Project", "Direct DB query works");

    // Try a simple endpoint that doesn't use CrudCrate
    let (status, body) = common::get_with_token(
        &app,
        &format!("/api/service/sites/{}/parameters", common::SITE1_ID),
        &token,
    )
    .await;
    eprintln!("Parameters endpoint: status={status}, body={body}");

    // Try the site detail endpoint (custom handler, not CrudCrate)
    let (status, body) = common::get_with_token(
        &app,
        &format!("/api/service/sites/{}", common::SITE1_ID),
        &token,
    )
    .await;
    assert_eq!(status, 200, "site detail should return 200, body: {body}");
    let json: serde_json::Value = serde_json::from_str(&body).expect("should be valid JSON");
    assert_eq!(json["name"].as_str().unwrap(), "Upstream Station");

    common::cleanup_test_db(&db).await;
}
