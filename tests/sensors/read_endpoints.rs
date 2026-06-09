//! The Phase 4 read substrate that drives the sensor-aware plot overlays: per-sensor series,
//! deployment bands, the calibration-window points, and the per-site sensor-identity (bands +
//! calibration markers keyed by parameter). Exercised over the real HTTP surface with a seeded token.
//!
//! Run: cargo test --test sensors -- --test-threads=1


use crate::common::sensor_lifecycle as sl;
use serial_test::serial;

#[tokio::test]
#[serial]
async fn sensor_read_substrate_endpoints() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor = sl::create_sensor(&db, "substrate", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let cal = sl::add_calibration(&db, sensor.id, 2.0, 1.0, sl::dt("2025-06-01T00:00:00Z")).await;
    let dep = sl::deploy_sensor(&db, sensor.id, crate::common::SITE1_ID, sl::dt("2025-06-01T00:00:00Z")).await;
    let stream = sl::create_paired_stream(&db, "substrate", crate::common::PARAM_S1_TEMP_ID).await;
    let raw: Vec<(_, f64)> = (0..6)
        .map(|i| (sl::dt(&format!("2025-06-01T00:{:02}:00Z", i * 10)), 10.0 + i as f64))
        .collect();
    sl::insert_readings(
        &db, stream, crate::common::SITE1_ID, crate::common::GLOBAL_PARAM_TEMP_ID, sensor.id, cal, dep, 2.0, 1.0, &raw,
    )
    .await;

    // /sensors/{id}/readings — columnar raw + calibrated aligned to times.
    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sensors/{}/readings", sensor.id),
        &token,
    )
    .await;
    assert_eq!(status, 200, "sensor readings: {body}");
    assert_eq!(body["sensor_id"], serde_json::json!(sensor.id.to_string()));
    let times = body["times"].as_array().unwrap();
    let calibrated = body["calibrated"].as_array().unwrap();
    let raws = body["raw"].as_array().unwrap();
    assert_eq!(times.len(), 6, "six readings");
    assert_eq!(raws[0].as_f64().unwrap(), 10.0);
    assert_eq!(calibrated[0].as_f64().unwrap(), 2.0 * 10.0 + 1.0, "y = 2x+1");
    assert_eq!(calibrated[5].as_f64().unwrap(), 2.0 * 15.0 + 1.0);

    // /sensors/{id}/deployment_bands — one band at site 1.
    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sensors/{}/deployment_bands", sensor.id),
        &token,
    )
    .await;
    assert_eq!(status, 200, "deployment bands: {body}");
    let bands = body["bands"].as_array().unwrap();
    assert_eq!(bands.len(), 1, "one deployment band");
    assert_eq!(bands[0]["site_id"], serde_json::json!(crate::common::SITE1_ID));
    assert_eq!(bands[0]["deployment_id"], serde_json::json!(dep.to_string()));

    // /sensor_calibrations/{id}/window — the readings the window resolves.
    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sensor_calibrations/{cal}/window"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "calibration window: {body}");
    assert_eq!(body["point_count"].as_i64().unwrap(), 6, "all six in the window");
    assert_eq!(body["slope"].as_f64().unwrap(), 2.0);
    assert_eq!(body["points"].as_array().unwrap().len(), 6);

    // /sites/{id}/sensor_identity — bands + calibration markers keyed by global parameter_id.
    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/sites/{}/sensor_identity?start=2025-05-01T00:00:00Z&end=2025-07-01T00:00:00Z",
            crate::common::SITE1_ID
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "sensor identity: {body}");
    let param_bands = body["bands"][crate::common::GLOBAL_PARAM_TEMP_ID].as_array().unwrap();
    assert_eq!(param_bands.len(), 1, "one deployment band for the parameter");
    assert_eq!(param_bands[0]["sensor_id"], serde_json::json!(sensor.id.to_string()));
    let param_cals = body["calibrations"][crate::common::GLOBAL_PARAM_TEMP_ID].as_array().unwrap();
    // The non-identity calibration (valid_from 2025-06-01) overlaps the window.
    assert!(
        param_cals.iter().any(|c| c["calibration_id"] == serde_json::json!(cal.to_string())),
        "the calibration marker is present: {body}"
    );
}
