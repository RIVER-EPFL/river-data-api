//! SensorDeployment API surface: `parameter_id` is serialized (trigger-maintained, read-only) so the
//! UI can resolve a slot's incumbent, and `deployed_until` is filterable so `deployed_until: null`
//! resolves the open deployments (the adopt/swap incumbent query).
//!
//! Run: cargo test --test sensor_deployment_api_test -- --test-threads=1

mod common;

use common::sensor_lifecycle as sl;
use serial_test::serial;

/// Percent-encode a JSON filter for the `?filter=` query param (no url crate in dev-deps).
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test]
#[serial]
async fn deployment_serializes_parameter_id() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());

    let sensor = sl::create_sensor(&db, "ser", common::GLOBAL_PARAM_TEMP_ID).await;
    let dep = sl::deploy_sensor(&db, sensor.id, common::SITE1_ID, sl::dt("2025-06-01T00:00:00Z")).await;

    let (status, body) =
        common::get_json_with_token(&app, &format!("/api/sensor_deployments/{dep}"), &token).await;
    assert_eq!(status, 200, "get deployment: {body}");
    assert_eq!(
        body["parameter_id"].as_str(),
        Some(common::GLOBAL_PARAM_TEMP_ID),
        "deployment must serialize the trigger-maintained parameter_id"
    );
}

#[tokio::test]
#[serial]
async fn deployed_until_null_filter_returns_only_open() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());

    let sensor = sl::create_sensor(&db, "filt", common::GLOBAL_PARAM_TEMP_ID).await;
    let closed =
        sl::deploy_sensor(&db, sensor.id, common::SITE1_ID, sl::dt("2025-06-01T00:00:00Z")).await;
    sl::end_deployment(&db, closed, sl::dt("2025-06-02T00:00:00Z")).await;
    let open =
        sl::deploy_sensor(&db, sensor.id, common::SITE2_ID, sl::dt("2025-06-02T00:00:00Z")).await;

    let filter = enc(&format!(
        r#"{{"sensor_id":"{}","deployed_until":null}}"#,
        sensor.id
    ));
    let (status, body) = common::get_json_with_token(
        &app,
        &format!("/api/sensor_deployments?filter={filter}"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "list open deployments: {body}");
    let arr = body.as_array().expect("list returns an array");
    assert_eq!(
        arr.len(),
        1,
        "only the open deployment matches deployed_until:null, got {body}"
    );
    assert_eq!(arr[0]["id"].as_str(), Some(open.to_string().as_str()));
    assert!(arr[0]["deployed_until"].is_null());
}
