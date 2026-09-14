//! An instrument's manufacturer measurement range is held on its own row.
//!
//! The range is entered by hand and rarely: no source feed supplies one. It records what the
//! manufacturer specifies the unit can measure, which is a different fact from the parameter's
//! own bounds, and a row that carries none behaves exactly as it did before.
//!
//! Run: cargo test --test sensors manufacturer_range -- --test-threads=1

use serde_json::json;
use serial_test::serial;

#[tokio::test]
#[serial]
async fn a_sensor_carries_the_range_its_manufacturer_specifies() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, created) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sensors",
        &json!({
            "serial_number": "RANGE-1",
            "manufacturer": "Vaisala",
            "range_min": -5.0,
            "range_max": 60.0,
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create ({status}): {created}");
    assert_eq!(created["range_min"], -5.0, "{created}");
    assert_eq!(created["range_max"], 60.0, "{created}");

    let id = created["id"].as_str().unwrap();
    let (status, fetched) =
        crate::common::get_json_with_token(&app, &format!("/api/sensors/{id}"), &token).await;
    assert_eq!(status, 200, "{fetched}");
    assert_eq!(fetched["range_min"], -5.0, "{fetched}");
    assert_eq!(fetched["range_max"], 60.0, "{fetched}");

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/sensors/{id}"),
        &json!({"range_max": 45.0}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "update ({status}): {body}");
    let updated: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(updated["range_max"], 45.0, "{updated}");
    assert_eq!(updated["range_min"], -5.0, "{updated}");
}

#[tokio::test]
#[serial]
async fn a_sensor_registered_without_a_range_reports_none() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, created) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sensors",
        &json!({"serial_number": "RANGE-2"}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create ({status}): {created}");
    assert!(created["range_min"].is_null(), "{created}");
    assert!(created["range_max"].is_null(), "{created}");
}
