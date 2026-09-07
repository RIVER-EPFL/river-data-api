//! The sensors list's latest-reading column, served from stream cursors and the hourly rollup
//! instead of an unbounded hypertable scan.
//!
//! Run: cargo test --test sensors list_latest_reading -- --test-threads=1

use sea_orm::DatabaseConnection;
use serde_json::json;
use serial_test::serial;

struct Fixture {
    db: DatabaseConnection,
    app: axum::Router,
    token: String,
}

async fn setup() -> Fixture {
    let f = crate::common::seeded_app().await;
    Fixture { db: f.db, app: f.app, token: f.token }
}

async fn create_sensor(fx: &Fixture, serial_number: &str) -> String {
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/sensors",
        &json!({"serial_number": serial_number, "manufacturer": "Test"}),
        &fx.token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create sensor ({status}): {body}"
    );
    body["id"].as_str().unwrap().to_string()
}

async fn sensor_row(fx: &Fixture, id: &str) -> serde_json::Value {
    let (status, body) =
        crate::common::get_json_with_token(&fx.app, "/api/sensors?page=1&per_page=100", &fx.token)
            .await;
    assert_eq!(status, 200, "sensors list ({status}): {body}");
    body.as_array()
        .expect("array body")
        .iter()
        .find(|s| s["id"] == id)
        .unwrap_or_else(|| panic!("sensor {id} missing from list"))
        .clone()
}

#[tokio::test]
#[serial]
async fn list_reads_latest_from_cursor_and_rollup() {
    // Scenario: one sensor's stream carries an ingest cursor; another's readings arrived by
    // batch, so only the hourly rollup knows about them.
    // Expected behaviour: both report their newest reading's time and value in the list.
    let fx = setup().await;

    let cursor_sensor = create_sensor(&fx, "CURSOR-1").await;
    let (status, stream) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/streams/register",
        &json!({"source_system": "vaisala", "source_key": "latest-cursor",
                "sensor_id": cursor_sensor}),
        &fx.token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "register ({status}): {stream}"
    );
    let stream_id = crate::common::e2e::id_of(&stream);
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/ingest",
        &json!({"stream_id": stream_id, "readings": [
            {"time": "2025-06-10T08:00:00Z", "raw_value": 1.5},
            {"time": "2025-06-10T09:00:00Z", "raw_value": 2.5},
        ]}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "ingest ({status}): {body}");

    let row = sensor_row(&fx, &cursor_sensor).await;
    assert_eq!(row["last_reading_at"], "2025-06-10T09:00:00Z", "{row}");
    assert_eq!(row["last_reading_value"], 2.5, "{row}");

    let batch_sensor = create_sensor(&fx, "BATCH-1").await;
    crate::common::exec(
        &fx.db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, sensor_id, time, raw_value) \
             SELECT id, '{}', '{}', '{batch_sensor}', '2025-01-15T06:05:30Z', 7.25 \
             FROM data_streams WHERE site_parameter_id = '{}'",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID,
            crate::common::PARAM_S1_TEMP_ID,
        ),
    )
    .await;
    crate::common::refresh_continuous_aggregates(&fx.db).await;

    let row = sensor_row(&fx, &batch_sensor).await;
    assert_eq!(
        row["last_reading_at"], "2025-01-15T06:05:30Z",
        "rollup fallback finds the batch-fed reading: {row}"
    );
    assert_eq!(row["last_reading_value"], 7.25, "{row}");
}

#[tokio::test]
#[serial]
async fn the_list_and_the_detail_report_one_sensor_alike() {
    // Expected behaviour: both read paths resolve the same enrichment, so a sensor's count and
    // latest reading do not depend on which endpoint asked.
    let fx = setup().await;

    let sensor = create_sensor(&fx, "SAME-1").await;
    for (time, value) in [("2025-01-16T07:00:30Z", 3.5), ("2025-01-16T08:00:30Z", 4.5)] {
        crate::common::exec(
            &fx.db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, sensor_id, time, raw_value) \
                 SELECT id, '{}', '{}', '{sensor}', '{time}', {value} \
                 FROM data_streams WHERE site_parameter_id = '{}'",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TEMP_ID,
                crate::common::PARAM_S1_TEMP_ID,
            ),
        )
        .await;
    }
    crate::common::refresh_continuous_aggregates(&fx.db).await;

    let listed = sensor_row(&fx, &sensor).await;
    let (status, detail) =
        crate::common::get_json_with_token(&fx.app, &format!("/api/sensors/{sensor}"), &fx.token)
            .await;
    assert_eq!(status, 200, "sensor detail ({status}): {detail}");

    for field in [
        "reading_count",
        "last_reading_at",
        "last_reading_value",
        "last_calibration_at",
        "current_site_id",
        "current_site_name",
    ] {
        assert_eq!(
            listed[field], detail[field],
            "{field} differs between the list and the detail: {listed} / {detail}"
        );
    }
    assert_eq!(detail["reading_count"], 2, "{detail}");
    assert_eq!(detail["last_reading_value"], 4.5, "{detail}");
}
