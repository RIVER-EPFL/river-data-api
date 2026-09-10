//! The deployment bodies the dashboard actually sends, verbatim. Every other test in the suite
//! builds a correct body from scratch, which is how a client omitting a required field stays
//! invisible; each test here mirrors one UI call site and must be updated with it.
//!
//! Run: cargo test --test sensor_deployments ui_shaped_bodies -- --test-threads=1

use crate::common::sensor_lifecycle as sl;
use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};
use serde_json::json;
use serial_test::serial;

struct Fixture {
    app: axum::Router,
    token: String,
    sensor_id: String,
}

async fn setup() -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let sensor = sl::create_sensor(&db, "UI-Body-01", GLOBAL_PARAM_TEMP_ID).await;
    Fixture {
        app,
        token,
        sensor_id: sensor.id.to_string(),
    }
}

/// `AdoptSensorDialog.svelte` and `DeployMoveSensorDialog.svelte`: `api.sensorDeployments.create`.
fn deploy_dialog_body(fx: &Fixture, deployment_type: &str) -> serde_json::Value {
    json!({
        "sensor_id": fx.sensor_id,
        "site_id": SITE1_ID,
        "parameter_id": GLOBAL_PARAM_TEMP_ID,
        "deployed_from": "2025-06-01T08:00:00.000Z",
        "deployment_type": deployment_type,
    })
}

async fn create(fx: &Fixture, body: &serde_json::Value) -> serde_json::Value {
    let (status, created) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/sensor_deployments",
        body,
        &fx.token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create ({status}): {created}, body: {body}"
    );
    created
}

async fn update(fx: &Fixture, id: &str, body: &serde_json::Value) -> serde_json::Value {
    let (status, text) = crate::common::put_json_with_token(
        &fx.app,
        &format!("/api/sensor_deployments/{id}"),
        body,
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "update ({status}): {text}, body: {body}");
    serde_json::from_str(&text).unwrap()
}

async fn reload(fx: &Fixture, id: &str) -> serde_json::Value {
    let (status, body) = crate::common::get_json_with_token(
        &fx.app,
        &format!("/api/sensor_deployments/{id}"),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "reload ({status}): {body}");
    body
}

#[tokio::test]
#[serial]
async fn the_deploy_dialogs_create_body_opens_a_deployment() {
    let fx = setup().await;
    for deployment_type in ["permanent", "field_campaign"] {
        let created = create(&fx, &deploy_dialog_body(&fx, deployment_type)).await;
        let id = created["id"].as_str().expect("id");
        let stored = reload(&fx, id).await;
        assert_eq!(stored["parameter_id"], GLOBAL_PARAM_TEMP_ID, "{stored}");
        assert_eq!(stored["deployment_type"], deployment_type, "{stored}");
        assert!(
            stored["deployed_until"].is_null(),
            "open on create: {stored}"
        );
    }
}

/// `sensors/[id]/+page.svelte` and `sites/[id]/+page.svelte` recall: `{ deployed_until: now }`.
#[tokio::test]
#[serial]
async fn the_recall_body_closes_the_deployment() {
    let fx = setup().await;
    let created = create(&fx, &deploy_dialog_body(&fx, "permanent")).await;
    let id = created["id"].as_str().expect("id");
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    update(&fx, id, &json!({ "deployed_until": now })).await;
    let stored = reload(&fx, id).await;
    assert!(stored["deployed_until"].is_string(), "recalled: {stored}");
}

/// `sensors/[id]/+page.svelte` backdate: `{ deployed_from: slot_data_start }`.
#[tokio::test]
#[serial]
async fn the_backdate_body_moves_deployed_from() {
    let fx = setup().await;
    let created = create(&fx, &deploy_dialog_body(&fx, "permanent")).await;
    let id = created["id"].as_str().expect("id");
    update(
        &fx,
        id,
        &json!({ "deployed_from": "2025-01-01T00:00:00.000Z" }),
    )
    .await;
    let stored = reload(&fx, id).await;
    assert!(
        stored["deployed_from"]
            .as_str()
            .unwrap()
            .starts_with("2025-01-01T00:00:00"),
        "{stored}"
    );
}

/// `sensors/[id]/+page.svelte` edit dates: both fields, `deployed_until` as JSON null when the
/// operator leaves the end blank. The null must clear an end that was set, not be ignored.
#[tokio::test]
#[serial]
async fn the_edit_dates_body_with_a_null_end_reopens_the_deployment() {
    let fx = setup().await;
    let created = create(&fx, &deploy_dialog_body(&fx, "permanent")).await;
    let id = created["id"].as_str().expect("id");
    update(
        &fx,
        id,
        &json!({ "deployed_until": "2025-07-01T00:00:00.000Z" }),
    )
    .await;
    assert!(reload(&fx, id).await["deployed_until"].is_string());

    update(
        &fx,
        id,
        &json!({ "deployed_from": "2025-05-01T00:00:00.000Z", "deployed_until": null }),
    )
    .await;
    let stored = reload(&fx, id).await;
    assert!(
        stored["deployed_from"]
            .as_str()
            .unwrap()
            .starts_with("2025-05-01T00:00:00"),
        "{stored}"
    );
    assert!(
        stored["deployed_until"].is_null(),
        "an explicit null reopens: {stored}"
    );
}
