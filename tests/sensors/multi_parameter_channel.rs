//! One sensor, two channels. A multi-parameter instrument holds one deployment per parameter, and
//! the per-sensor series must be about exactly one of them: the one the response names, whose
//! units it reports, and whose extent it advertises.
//!
//! Run: cargo test --test sensors multi_parameter_channel -- --test-threads=1

use crate::common::db::exec;
use crate::common::sensor_lifecycle as sl;
use serial_test::serial;
use uuid::Uuid;

/// A second deployment for the same sensor on another parameter, which `deploy_sensor` cannot
/// express (it derives the parameter from the sensor's identity calibration).
async fn deploy_on_parameter(
    db: &sea_orm::DatabaseConnection,
    sensor_id: Uuid,
    site_id: &str,
    parameter_id: &str,
    from: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    exec(
        db,
        &format!(
            "INSERT INTO sensor_deployments \
             (id, sensor_id, site_id, parameter_id, deployed_from, deployment_type) \
             VALUES ('{id}', '{sensor_id}', '{site_id}', '{parameter_id}', '{from}', 'permanent')"
        ),
    )
    .await;
    id
}

/// Conductivity near 500 and temperature near 10 on one instrument, the two quantities whose
/// average is meaningless if they share an array. Returns the sensor id.
async fn seed_two_channel_sensor(db: &sea_orm::DatabaseConnection) -> Uuid {
    let sensor = sl::create_sensor(db, "two-channel", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let temp_deployment = sl::deploy_sensor(
        db,
        sensor.id,
        crate::common::SITE1_ID,
        sl::dt("2025-06-01T00:00:00Z"),
    )
    .await;
    let cond_deployment = deploy_on_parameter(
        db,
        sensor.id,
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_COND_ID,
        "2025-06-01T01:00:00Z",
    )
    .await;

    let temp_stream =
        sl::create_paired_stream(db, "two-channel-temp", crate::common::PARAM_S1_TEMP_ID).await;
    let cond_stream =
        sl::create_paired_stream(db, "two-channel-cond", crate::common::PARAM_S1_COND_ID).await;

    let temp: Vec<(_, f64)> = (0..3)
        .map(|i| {
            (
                sl::dt(&format!("2025-06-02T0{i}:00:00Z")),
                10.0 + f64::from(i),
            )
        })
        .collect();
    let cond: Vec<(_, f64)> = (0..3)
        .map(|i| {
            (
                sl::dt(&format!("2025-06-02T0{}:00:00Z", i + 3)),
                500.0 + f64::from(i),
            )
        })
        .collect();

    sl::insert_readings(
        db,
        temp_stream,
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        sensor.id,
        sensor.identity_calibration_id,
        temp_deployment,
        1.0,
        0.0,
        &temp,
    )
    .await;
    sl::insert_readings(
        db,
        cond_stream,
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_COND_ID,
        sensor.id,
        sensor.identity_calibration_id,
        cond_deployment,
        1.0,
        0.0,
        &cond,
    )
    .await;

    sensor.id
}

const WINDOW: &str = "start=2025-06-02T00:00:00Z&end=2025-06-02T23:59:59Z";

fn floats(body: &serde_json::Value, key: &str) -> Vec<f64> {
    body[key]
        .as_array()
        .unwrap_or_else(|| panic!("no '{key}' array in {body}"))
        .iter()
        .map(|v| v.as_f64().unwrap_or(f64::NAN))
        .collect()
}

#[tokio::test]
#[serial]
async fn the_default_channel_is_the_latest_deployments_parameter_and_the_series_holds_only_it() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor_id = seed_two_channel_sensor(&db).await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sensors/{}/readings?{WINDOW}", sensor_id),
        &token,
    )
    .await;
    assert_eq!(status, 200, "sensor readings ({status}): {body}");
    assert_eq!(
        body["parameter_id"].as_str(),
        Some(crate::common::GLOBAL_PARAM_COND_ID),
        "the later deployment's parameter is the reported identity: {body}"
    );
    assert_eq!(
        body["units"].as_str(),
        Some("µS/cm"),
        "and the units belong to it: {body}"
    );
    assert_eq!(
        floats(&body, "raw"),
        vec![500.0, 501.0, 502.0],
        "only the named channel's readings are in the array: {body}"
    );
    assert_eq!(
        body["times"].as_array().map(Vec::len),
        Some(3),
        "the other channel's timestamps are not on the axis: {body}"
    );
}

#[tokio::test]
#[serial]
async fn the_parameter_id_selector_serves_the_other_channel_with_its_own_units() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor_id = seed_two_channel_sensor(&db).await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/sensors/{}/readings?{WINDOW}&parameter_id={}",
            sensor_id,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "temperature channel ({status}): {body}");
    assert_eq!(
        body["parameter_id"].as_str(),
        Some(crate::common::GLOBAL_PARAM_TEMP_ID),
        "the selector decides the identity: {body}"
    );
    assert_eq!(
        body["units"].as_str(),
        Some("°C"),
        "with its own units: {body}"
    );
    assert_eq!(
        floats(&body, "raw"),
        vec![10.0, 11.0, 12.0],
        "and its own values: {body}"
    );
}

#[tokio::test]
#[serial]
async fn a_bucketed_read_averages_one_quantity() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor_id = seed_two_channel_sensor(&db).await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/sensors/{}/readings?{WINDOW}&resolution=daily",
            sensor_id
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "daily sensor readings ({status}): {body}");
    assert_eq!(
        body["parameter_id"].as_str(),
        Some(crate::common::GLOBAL_PARAM_COND_ID),
        "the bucketed arm names the same parameter as the raw arm: {body}"
    );
    assert_eq!(
        floats(&body, "raw"),
        vec![501.0],
        "the six readings fall on one day, and the mean is conductivity's alone \
         (mixing in 10/11/12 would give 255.5): {body}"
    );
}

#[tokio::test]
#[serial]
async fn the_temporal_extent_is_scoped_to_the_served_channel() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor_id = seed_two_channel_sensor(&db).await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sensors/{}/readings", sensor_id),
        &token,
    )
    .await;
    assert_eq!(status, 200, "sensor readings ({status}): {body}");
    assert!(
        body["data_start"]
            .as_str()
            .unwrap_or_default()
            .starts_with("2025-06-02T03:00:00"),
        "the extent starts at conductivity's first reading, not temperature's: {body}"
    );
    assert!(
        body["data_end"]
            .as_str()
            .unwrap_or_default()
            .starts_with("2025-06-02T05:00:00"),
        "and ends at its last: {body}"
    );
}

#[tokio::test]
#[serial]
async fn a_sensor_with_no_deployment_still_serves_what_is_attributed_to_it() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor = sl::create_sensor(&db, "inventory", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let stream =
        sl::create_paired_stream(&db, "inventory-temp", crate::common::PARAM_S1_TEMP_ID).await;
    exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, \
             calibrated_value, sensor_id, replicate_index) VALUES \
             ('{stream}', '{site}', '{param}', '2025-06-02T00:00:00Z', 7.0, 7.0, '{}', 0)",
            sensor.id,
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sensors/{}/readings?{WINDOW}", sensor.id),
        &token,
    )
    .await;
    assert_eq!(status, 200, "undeployed sensor readings ({status}): {body}");
    assert!(
        body["parameter_id"].is_null(),
        "an undeployed instrument names no parameter: {body}"
    );
    assert_eq!(
        floats(&body, "raw"),
        vec![7.0],
        "and the plot is not left empty by a filter it cannot resolve: {body}"
    );
}

#[tokio::test]
#[serial]
async fn an_unknown_parameter_selector_is_rejected() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor_id = seed_two_channel_sensor(&db).await;

    let (status, body) = crate::common::get_with_token(
        &app,
        &format!(
            "/api/sensors/{sensor_id}/readings?parameter_id=00000000-0000-4000-b000-0000000000ff"
        ),
        &token,
    )
    .await;
    assert_eq!(status, 400, "an unknown parameter is a bad request: {body}");
}
