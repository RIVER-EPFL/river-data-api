//! `GET /sites/{id}/last_curve`: the instrument and standard curve the newest grab at a site
//! and parameter recorded, which the curve picker opens on.
//!
//! Scenario: two DOC batches are entered at one site on different days against different curves of
//! the same plate reader, then a temperature grab names the instrument alone.
//! Expected behaviour: the lookup at (site, DOC) answers the second batch's curve and its
//! instrument; at (site, temperature) it answers the instrument with no curve; at a parameter
//! nothing was entered for, both are null; and the response says how it decided.

use crate::common::sensor_lifecycle::*;
use crate::common::*;
use sea_orm::ConnectionTrait;
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

struct Fixture {
    app: axum::Router,
    token: String,
    db: sea_orm::DatabaseConnection,
}

async fn setup() -> Fixture {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    let token = seed_api_token(&db, full_permissions(), None).await;
    let app = build_test_app(db.clone());
    Fixture { app, token, db }
}

async fn create_curve(fx: &Fixture, sensor_id: Uuid, name: &str) -> Uuid {
    let (status, body) = post_json_parse_with_token(
        &fx.app,
        "/api/standard_curves",
        &json!({ "sensor_id": sensor_id, "name": name, "slope": 2.0, "intercept": 1.0 }),
        &fx.token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "creating curve '{name}': {body}"
    );
    body["id"].as_str().unwrap().parse().unwrap()
}

async fn post_grab(fx: &Fixture, reading: serde_json::Value) {
    let (status, body) = crate::common::post_checked_grab(
        &fx.app,
        &json!({ "site_id": SITE1_ID, "readings": [reading] }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "grab should be stored: {body}");
}

async fn last_used(fx: &Fixture, parameter_id: &str) -> serde_json::Value {
    let (status, body) = get_with_token(
        &fx.app,
        &format!("/api/sites/{SITE1_ID}/last_curve?parameter_id={parameter_id}"),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "lookup should answer: {body}");
    serde_json::from_str(&body).expect("lookup body is JSON")
}

#[tokio::test]
#[serial]
async fn the_newest_grab_at_the_slot_names_the_default_instrument_and_curve() {
    let fx = setup().await;
    let reader = create_sensor(&fx.db, "Plate reader", GLOBAL_PARAM_DO_ID).await;
    let first = create_curve(&fx, reader.id, "Plate 2025-06-01").await;
    let second = create_curve(&fx, reader.id, "Plate 2025-06-08").await;

    post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_DO_ID,
            "sensor_id": reader.id,
            "standard_curve_id": first,
            "value": 10.0,
            "time": "2025-06-01T10:00:00Z",
        }),
    )
    .await;
    post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_DO_ID,
            "sensor_id": reader.id,
            "standard_curve_id": second,
            "value": 12.0,
            "time": "2025-06-08T10:00:00Z",
        }),
    )
    .await;
    post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "sensor_id": reader.id,
            "value": 7.5,
            "time": "2025-06-09T10:00:00Z",
        }),
    )
    .await;

    let doc = last_used(&fx, GLOBAL_PARAM_DO_ID).await;
    assert_eq!(doc["sensor_id"], json!(reader.id), "{doc}");
    assert_eq!(doc["sensor_name"], json!("Plate reader"), "{doc}");
    assert_eq!(
        doc["standard_curve_id"],
        json!(second),
        "the second batch's curve: {doc}"
    );
    assert_eq!(doc["curve_name"], json!("Plate 2025-06-08"), "{doc}");
    assert_eq!(doc["used_at"], json!("2025-06-08T10:00:00Z"), "{doc}");
    assert!(
        doc["method"].as_str().is_some_and(|m| m.contains("newest")),
        "the response explains how it decided: {doc}"
    );

    let temp = last_used(&fx, GLOBAL_PARAM_TEMP_ID).await;
    assert_eq!(temp["sensor_id"], json!(reader.id), "{temp}");
    assert_eq!(
        temp["standard_curve_id"],
        json!(null),
        "no curve was recorded: {temp}"
    );
    assert_eq!(temp["used_at"], json!("2025-06-09T10:00:00Z"), "{temp}");

    let none = last_used(&fx, GLOBAL_PARAM_COND_ID).await;
    assert_eq!(none["sensor_id"], json!(null), "{none}");
    assert_eq!(none["standard_curve_id"], json!(null), "{none}");
    assert_eq!(none["used_at"], json!(null), "{none}");
}

#[tokio::test]
#[serial]
async fn a_curve_is_found_by_the_parameter_code_too() {
    let fx = setup().await;
    let reader = create_sensor(&fx.db, "Plate reader", GLOBAL_PARAM_DO_ID).await;
    let curve = create_curve(&fx, reader.id, "Plate A").await;
    post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_DO_ID,
            "sensor_id": reader.id,
            "standard_curve_id": curve,
            "value": 10.0,
            "time": "2025-06-01T10:00:00Z",
        }),
    )
    .await;

    let code = fx
        .db
        .query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT code FROM parameters WHERE id = '{GLOBAL_PARAM_DO_ID}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<String>("", "code")
        .unwrap();
    let (status, body) = get_json_with_token(
        &fx.app,
        &format!("/api/sites/{SITE1_ID}/last_curve?parameter_code={code}"),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["standard_curve_id"], json!(curve), "{body}");
    assert_eq!(body["parameter_id"], json!(GLOBAL_PARAM_DO_ID), "{body}");

    let (status, body) = get_with_token(
        &fx.app,
        &format!("/api/sites/{SITE1_ID}/last_curve"),
        &fx.token,
    )
    .await;
    assert_eq!(
        status, 400,
        "a lookup naming no parameter is refused: {body}"
    );
}
