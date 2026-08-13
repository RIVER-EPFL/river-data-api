//! Regression tests for the site_parameter create-friction fix.
//!
//! Assigning a parameter to a site should require only `{site_id, parameter_id}`:
//! `name` is backfilled from the parameter's human label (`parameters.name`) and
//! `sensor_type` defaults to an empty string (the API falls back to the parameter
//! code for display). Run with: cargo test --test site_parameters

use serial_test::serial;

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

#[tokio::test]
#[serial]
async fn create_site_parameter_with_only_site_and_parameter() {
    let (_db, app, token) = setup().await;

    // SITE2 has no Depth assigned in the seed, so this is a clean (site, parameter) pair.
    let body = serde_json::json!({
        "site_id": crate::common::SITE2_ID,
        "parameter_id": crate::common::GLOBAL_PARAM_DEPTH_ID,
    });
    let (status, text) =
        crate::common::post_json_with_token(&app, "/api/site_parameters", &body, &token).await;
    assert!(
        (200..300).contains(&status),
        "minimal create should succeed, got {status}: {text}"
    );

    let json: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    let id = json["id"].as_str().expect("response should have id");

    let (gstatus, gtext) =
        crate::common::get_with_token(&app, &format!("/api/site_parameters/{id}"), &token).await;
    assert_eq!(gstatus, 200, "GET should succeed: {gtext}");
    let got: serde_json::Value = serde_json::from_str(&gtext).expect("valid json");
    assert_eq!(
        got["name"], "Water Depth",
        "name should be backfilled from the parameter's human label (parameters.name): {got}"
    );
    assert_eq!(
        got["sensor_type"], "",
        "sensor_type should default to an empty string: {got}"
    );
}
