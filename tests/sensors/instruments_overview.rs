//! `GET /instruments/overview` and `GET /standard_curves/{id}/usage`: the read surface behind the
//! streams page's Instruments tab. An instrument appears once it owns a curve or feeds a stream;
//! each curve reports how many readings it corrected and the drill-down lists them.
//!
//! Run: cargo test --test sensors instruments_overview -- --test-threads=1

use serde_json::json;
use serial_test::serial;

const T1: &str = "2025-06-10T08:00:00Z";

struct Fixture {
    app: axum::Router,
    token: String,
}

async fn setup() -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    Fixture { app, token }
}

#[tokio::test]
#[serial]
async fn overview_lists_instruments_curves_and_their_readings() {
    let fx = setup().await;

    let (status, curve) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/standard_curves/register",
        &json!({
            "source_system": "cnet",
            "source_key": "standard_curves:3",
            "instrument_label": "DOC corr",
            "slope": 2.0,
            "intercept": 1.0,
            "name": "DOC corr 2021-01-28",
        }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "register curve ({status}): {curve}");
    let curve_id = curve["id"].as_str().unwrap().to_string();
    let sensor_id = curve["sensor_id"].as_str().unwrap().to_string();

    let (status, stream) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/streams/register",
        &json!({"source_system": "cnet", "source_key": "FP1:DOC_avg_ppb:reps",
                "measurement_type": "spot", "sensor_id": sensor_id}),
        &fx.token,
    )
    .await;
    assert!((200..300).contains(&status), "register stream: {stream}");
    let stream_id = crate::common::e2e::id_of(&stream);
    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({"site_parameter_id": crate::common::PARAM_S1_TEMP_ID}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "pair ({status}): {body}");

    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/ingest",
        &json!({"stream_id": stream_id, "readings": [
            {"time": T1, "raw_value": 10.0, "replicate_index": 0, "standard_curve_id": curve_id}
        ]}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "ingest ({status}): {body}");
    assert_eq!(body["inserted"], 1, "{body}");

    let (status, overview) =
        crate::common::get_json_with_token(&fx.app, "/api/instruments/overview", &fx.token).await;
    assert_eq!(status, 200, "overview ({status}): {overview}");
    let instruments = overview["instruments"].as_array().unwrap();
    let inst = instruments
        .iter()
        .find(|i| i["id"] == json!(sensor_id))
        .expect("the curve's lab instrument is listed");
    assert_eq!(inst["is_lab_instrument"], true, "{inst}");
    assert_eq!(inst["source_system"], "cnet", "{inst}");

    let curves = inst["curves"].as_array().unwrap();
    assert_eq!(curves.len(), 1, "{inst}");
    assert_eq!(curves[0]["id"], json!(curve_id));
    assert_eq!(curves[0]["source_key"], "standard_curves:3");
    assert_eq!(
        curves[0]["reading_count"], 1,
        "the corrected reading counts against its curve: {inst}"
    );
    assert_eq!(curves[0]["first_used"], json!("2025-06-10T08:00:00Z"));

    let streams = inst["streams"].as_array().unwrap();
    assert_eq!(streams.len(), 1, "{inst}");
    assert_eq!(streams[0]["source_key"], "FP1:DOC_avg_ppb:reps");
    assert_eq!(
        streams[0]["parameter_code"].as_str().map(str::is_empty),
        Some(false),
        "the paired slot resolves a parameter code: {inst}"
    );

    let (status, usage) = crate::common::get_json_with_token(
        &fx.app,
        &format!("/api/standard_curves/{curve_id}/usage"),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "usage ({status}): {usage}");
    assert_eq!(usage["reading_count"], 1, "{usage}");
    let point = &usage["points"][0];
    assert_eq!(point["raw_value"], 10.0, "{usage}");
    assert_eq!(
        point["calibrated_value"], 21.0,
        "corrected = slope * raw + intercept: {usage}"
    );
    assert_eq!(
        point["site_name"].as_str().map(str::is_empty),
        Some(false),
        "{usage}"
    );
}
