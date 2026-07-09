//! Instant (lab grab) curves are created through the ordinary `sensor_calibrations` CRUD endpoint:
//! `mode` defaults to `windowed` when omitted and accepts an explicit `instant` override so the
//! grab-entry "add curve" affordance can mint a lab curve without a bespoke endpoint.

use crate::common::sensor_lifecycle::*;
use crate::common::*;
use serial_test::serial;

#[tokio::test]
#[serial]
async fn calibration_create_defaults_windowed_and_accepts_instant_override() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Microplate-01", GLOBAL_PARAM_TEMP_ID).await;
    let app = build_test_app(db.clone());
    let token = seed_token_full(&db).await;

    let (status, json) = post_json_parse_with_token(
        &app,
        "/api/sensor_calibrations",
        &serde_json::json!({
            "sensor_id": sensor.id,
            "slope": 2.0,
            "intercept": 1.0,
            "valid_from": "2025-06-01T00:00:00Z",
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "windowed create should succeed: {json}");
    assert_eq!(json["mode"], "windowed", "omitting mode defaults to windowed");

    let (status, json) = post_json_parse_with_token(
        &app,
        "/api/sensor_calibrations",
        &serde_json::json!({
            "sensor_id": sensor.id,
            "name": "Plate A",
            "mode": "instant",
            "slope": 0.5,
            "intercept": 0.25,
            "valid_from": "2025-06-01T00:00:00Z",
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "instant create should succeed: {json}");
    assert_eq!(json["mode"], "instant", "explicit mode override is honoured");
    assert_eq!(json["name"], "Plate A");
    assert!(
        json["parameter_id"].is_null(),
        "an instant lab curve stays parameter-free (parameter is decided at the grab): {json}"
    );
}
