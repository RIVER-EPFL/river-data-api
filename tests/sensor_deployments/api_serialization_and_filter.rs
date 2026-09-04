//! SensorDeployment API surface: `parameter_id` is authored at create and serialized back so the
//! UI can resolve a slot's incumbent, and `deployed_until` is filterable so `deployed_until: null`
//! resolves the open deployments (the adopt/swap incumbent query).
//!
//! Run: cargo test --test sensor_deployments -- --test-threads=1

use crate::common::sensor_lifecycle as sl;
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
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor = sl::create_sensor(&db, "ser", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let dep = sl::deploy_sensor(
        &db,
        sensor.id,
        crate::common::SITE1_ID,
        sl::dt("2025-06-01T00:00:00Z"),
    )
    .await;

    let (status, body) =
        crate::common::get_json_with_token(&app, &format!("/api/sensor_deployments/{dep}"), &token)
            .await;
    assert_eq!(status, 200, "get deployment: {body}");
    assert_eq!(
        body["parameter_id"].as_str(),
        Some(crate::common::GLOBAL_PARAM_TEMP_ID),
        "deployment must serialize the parameter it was created with"
    );
}

#[tokio::test]
#[serial]
async fn deployed_until_null_filter_returns_only_open() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor = sl::create_sensor(&db, "filt", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let closed = sl::deploy_sensor(
        &db,
        sensor.id,
        crate::common::SITE1_ID,
        sl::dt("2025-06-01T00:00:00Z"),
    )
    .await;
    sl::end_deployment(&db, closed, sl::dt("2025-06-02T00:00:00Z")).await;
    let open = sl::deploy_sensor(
        &db,
        sensor.id,
        crate::common::SITE2_ID,
        sl::dt("2025-06-02T00:00:00Z"),
    )
    .await;

    let filter = enc(&format!(
        r#"{{"sensor_id":"{}","deployed_until":null}}"#,
        sensor.id
    ));
    let (status, body) = crate::common::get_json_with_token(
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

/// Scenario: the body the deploy, move and adopt dialogs send.
///
/// Expected behaviour: a deployment binds a sensor to one parameter at a site, so a create without
/// `parameter_id` is refused and the same body carrying it succeeds. Nothing derives the parameter
/// from the sensor any more, so a client that omits it has no deployment.
#[tokio::test]
#[serial]
async fn a_deployment_create_without_a_parameter_is_refused() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let sensor = sl::create_sensor(&db, "ui-shaped", crate::common::GLOBAL_PARAM_TEMP_ID).await;

    let mut body = serde_json::json!({
        "sensor_id": sensor.id,
        "site_id": crate::common::SITE1_ID,
        "deployed_from": "2025-06-01T00:00:00Z",
        "deployment_type": "permanent",
    });
    let (status, refused) =
        crate::common::post_json_with_token(&app, "/api/sensor_deployments", &body, &token).await;
    assert!(
        (400..500).contains(&status),
        "a body without parameter_id is refused ({status}): {refused}"
    );

    body["parameter_id"] = serde_json::json!(crate::common::GLOBAL_PARAM_TEMP_ID);
    let (status, created) =
        crate::common::post_json_parse_with_token(&app, "/api/sensor_deployments", &body, &token)
            .await;
    assert!(
        (200..300).contains(&status),
        "the same body naming the parameter is accepted ({status}): {created}"
    );
    assert_eq!(
        created["parameter_id"].as_str(),
        Some(crate::common::GLOBAL_PARAM_TEMP_ID),
        "{created}"
    );
}

/// Scenario: an `IN` list over a timestamptz column, the shape a client sends to ask for several
/// named instants at once.
///
/// Expected behaviour: the elements bind as timestamps. Bound as text, Postgres answers
/// `operator does not exist: timestamp with time zone = text` and the request 500s.
#[tokio::test]
#[serial]
async fn deployed_until_array_filter_selects_the_listed_instants() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor = sl::create_sensor(&db, "arrfilt", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    for (from, until) in [
        ("2025-06-01T00:00:00Z", "2025-06-02T00:00:00Z"),
        ("2025-06-02T00:00:00Z", "2025-06-03T00:00:00Z"),
        ("2025-06-03T00:00:00Z", "2025-06-04T00:00:00Z"),
    ] {
        let id = sl::deploy_sensor(&db, sensor.id, crate::common::SITE1_ID, sl::dt(from)).await;
        sl::end_deployment(&db, id, sl::dt(until)).await;
    }

    let filter = enc(&format!(
        r#"{{"sensor_id":"{}","deployed_until":["2025-06-02T00:00:00Z","2025-06-04T00:00:00Z"]}}"#,
        sensor.id
    ));
    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sensor_deployments?filter={filter}"),
        &token,
    )
    .await;

    assert_eq!(status, 200, "timestamptz IN list must not error: {body}");
    let arr = body.as_array().expect("list returns an array");
    assert_eq!(arr.len(), 2, "expected the two listed instants, got {body}");
}

/// Expected behaviour: an element that is not a timestamp is refused, rather than dropping the
/// clause and answering with every deployment.
#[tokio::test]
#[serial]
async fn deployed_until_array_filter_refuses_an_unparseable_element() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor = sl::create_sensor(&db, "badfilt", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let id = sl::deploy_sensor(
        &db,
        sensor.id,
        crate::common::SITE1_ID,
        sl::dt("2025-06-01T00:00:00Z"),
    )
    .await;
    sl::end_deployment(&db, id, sl::dt("2025-06-02T00:00:00Z")).await;

    let filter = enc(&format!(
        r#"{{"sensor_id":"{}","deployed_until":["2025-06-02T00:00:00Z","not a timestamp"]}}"#,
        sensor.id
    ));
    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sensor_deployments?filter={filter}"),
        &token,
    )
    .await;

    assert_eq!(status, 400, "an unfilterable value is refused: {body}");
}
