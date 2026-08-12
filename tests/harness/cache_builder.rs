//! Scenario: the cached builders switch the response cache on, while the default builders keep the
//! cacheless behaviour every other theme relies on.

use serial_test::serial;

use crate::common::{
    PARAM_S1_TEMP_ID, PROJECT_ID, SITE1_ID, build_test_app, build_test_app_with_cache,
    cleanup_test_db, exec, get_json, seed_test_data, setup_test_db,
};

const READINGS_URI: &str = "/api/public/test-river/sites/upstream/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T01:00:00Z";

async fn seed_public(db: &sea_orm::DatabaseConnection) {
    cleanup_test_db(db).await;
    seed_test_data(db).await;
    exec(
        db,
        &format!(
            "UPDATE projects SET is_public = true, public_code = 'test-river' WHERE id = '{PROJECT_ID}'"
        ),
    )
    .await;
    exec(
        db,
        &format!("UPDATE sites SET public_code = 'upstream' WHERE id = '{SITE1_ID}'"),
    )
    .await;
    exec(
        db,
        &format!("UPDATE site_parameters SET is_public = true WHERE id = '{PARAM_S1_TEMP_ID}'"),
    )
    .await;
}

#[tokio::test]
#[serial]
async fn test_cached_builder_serves_a_second_bounded_query_from_cache() {
    let db = setup_test_db().await;
    seed_public(&db).await;
    let app = build_test_app_with_cache(db.clone());

    let (status, first) = get_json(&app, READINGS_URI).await;
    assert_eq!(status, 200, "{first}");
    assert!(
        first["parameters"][0]["values"]
            .as_array()
            .is_some_and(|v| !v.is_empty()),
        "fixture should return values: {first}"
    );

    exec(&db, "DELETE FROM readings").await;
    let (status, second) = get_json(&app, READINGS_URI).await;
    assert_eq!(status, 200);
    assert_eq!(first, second, "the second GET should be served from cache");

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn test_default_builder_stays_cacheless() {
    let db = setup_test_db().await;
    seed_public(&db).await;
    let app = build_test_app(db.clone());

    let (_, first) = get_json(&app, READINGS_URI).await;
    exec(&db, "DELETE FROM readings").await;
    let (_, second) = get_json(&app, READINGS_URI).await;
    assert_ne!(first, second, "the default builder must not cache");

    cleanup_test_db(&db).await;
}
