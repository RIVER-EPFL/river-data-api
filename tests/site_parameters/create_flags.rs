//! `is_active` and `is_public` on create.
//!
//! Both carry a create-time default and both are settable by the client on the same request, so
//! the create must not decide either of them on the client's behalf when the client said
//! something. Run with: cargo test --test site_parameters

use serde_json::json;
use serial_test::serial;

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

async fn create(app: &axum::Router, token: &str, body: &serde_json::Value) -> serde_json::Value {
    let (status, text) =
        crate::common::post_json_with_token(app, "/api/site_parameters", body, token).await;
    assert!(
        (200..300).contains(&status),
        "create ({status}): {text}, body: {body}"
    );
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("create body is not JSON: {e}: {text}"))
}

async fn reload(app: &axum::Router, token: &str, created: &serde_json::Value) -> serde_json::Value {
    let id = created["id"].as_str().expect("created id");
    let (status, text) =
        crate::common::get_with_token(app, &format!("/api/site_parameters/{id}"), token).await;
    assert_eq!(status, 200, "reload ({status}): {text}");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("reload body is not JSON: {e}: {text}"))
}

#[tokio::test]
#[serial]
async fn the_public_toggle_survives_the_create() {
    let (_db, app, token) = setup().await;

    let public = create(
        &app,
        &token,
        &json!({
            "site_id": crate::common::SITE2_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_DEPTH_ID,
            "is_public": true,
        }),
    )
    .await;
    assert_eq!(
        public["is_public"],
        json!(true),
        "the response carries the toggle the caller sent: {public}"
    );
    assert_eq!(
        reload(&app, &token, &public).await["is_public"],
        json!(true),
        "and the stored row does too"
    );
}

#[tokio::test]
#[serial]
async fn an_omitted_toggle_creates_a_private_active_slot() {
    let (_db, app, token) = setup().await;

    let created = create(
        &app,
        &token,
        &json!({
            "site_id": crate::common::SITE2_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_DEPTH_ID,
        }),
    )
    .await;
    assert_eq!(
        created["is_public"],
        json!(false),
        "a slot is private until someone says otherwise: {created}"
    );
    assert_eq!(
        created["is_active"],
        json!(true),
        "a slot is active on creation: {created}"
    );

    let stored = reload(&app, &token, &created).await;
    assert_eq!(
        stored["is_public"],
        json!(false),
        "stored private: {stored}"
    );
    assert_eq!(stored["is_active"], json!(true), "stored active: {stored}");
}

#[tokio::test]
#[serial]
async fn an_explicit_false_is_not_overwritten_by_the_default() {
    let (_db, app, token) = setup().await;

    let created = create(
        &app,
        &token,
        &json!({
            "site_id": crate::common::SITE2_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_DEPTH_ID,
            "is_active": false,
            "is_public": false,
        }),
    )
    .await;
    assert_eq!(
        created["is_active"],
        json!(false),
        "a slot created inactive stays inactive: {created}"
    );

    let stored = reload(&app, &token, &created).await;
    assert_eq!(
        stored["is_active"],
        json!(false),
        "stored inactive: {stored}"
    );
    assert_eq!(
        stored["is_public"],
        json!(false),
        "stored private: {stored}"
    );
}
