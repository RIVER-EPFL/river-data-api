//! Every private export arm carries the stored `DOUBLE PRECISION` value unchanged: take the number
//! text the arm emits, parse it back to `f64`, and it is bit-for-bit the number that was stored
//! (Q31, T14). A fixed-places formatter anywhere in one of these paths is a silent precision loss,
//! so the assertion is `to_bits` equality, not approximate.
//!
//! The three fixture values each need all 17 significant digits and each differ from their
//! 2-decimal rounding (the slot declares `decimal_places = 2`), so a `{:.6}`-style formatter would
//! pass an approximate test yet fail these.
//!
//! The emitted number text is captured with `RawValue` and parsed with the standard-library `f64`
//! parser. `serde_json::Value::as_f64` is not used: its float parser is off by one ULP on the
//! f32-origin fixture, which would blame the export for a defect in the test's own reparse. The
//! text the arms emit (ryu for JSON/NDJSON, `f64::to_string` for CSV) is the canonical shortest
//! round-trip and recovers the stored double exactly under a correct parser.
//!
//! Run: cargo test --test sites export_double_roundtrip -- --test-threads=1

use serde_json::value::RawValue;
use serial_test::serial;

/// Shortest-round-trip cases: `0.1 + 0.2`, an arithmetic result, and a value widened from f32
/// (the corrected-value-from-single-precision case the register calls out).
fn fixture_values() -> [f64; 3] {
    [0.1_f64 + 0.2, 1683.4228_f64 * 1.3642 + -1.6985, f64::from(100.8_f32)]
}

const T0: &str = "2025-06-01T00:00:00Z";
const T1: &str = "2025-06-01T00:10:00Z";
const T2: &str = "2025-06-01T00:20:00Z";
const WINDOW: &str = "start=2025-06-01T00:00:00Z&end=2025-06-01T01:00:00Z";
// Stream 0 in the seed is SITE1 / DO_Temperature, already paired to its site_parameter.
const TEMP_STREAM: &str = "00000000-0000-4000-d000-000000000001";

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

/// Insert the three fixture values as continuous temperature readings at T0/T1/T2.
async fn insert_fixture_readings(db: &sea_orm::DatabaseConnection) {
    let [v0, v1, v2] = fixture_values();
    for (time, value) in [(T0, v0), (T1, v1), (T2, v2)] {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO readings \
                    (stream_id, site_id, parameter_id, time, replicate_index, raw_value, \
                     measurement_type) \
                 VALUES ('{TEMP_STREAM}', '{site}', '{param}', '{time}', 0, {value:?}, \
                         'continuous')",
                site = crate::common::SITE1_ID,
                param = crate::common::GLOBAL_PARAM_TEMP_ID,
            ),
        )
        .await;
    }
}

fn readings_uri(fmt: &str) -> String {
    format!(
        "/api/sites/{site}/readings?{WINDOW}&parameter_ids={param}&format={fmt}",
        site = crate::common::SITE1_ID,
        param = crate::common::GLOBAL_PARAM_TEMP_ID,
    )
}

/// Parse an emitted number token with the standard-library parser (a correct, round-tripping f64
/// parser), never `serde_json::Value::as_f64`.
fn parse_std(text: &str) -> f64 {
    text.trim()
        .parse()
        .unwrap_or_else(|e| panic!("parse f64 from {text:?}: {e}"))
}

fn assert_bits_eq(parsed: f64, stored: f64, context: &str) {
    assert_eq!(
        parsed.to_bits(),
        stored.to_bits(),
        "{context}: exported {parsed:?} (bits {:#018x}) != stored {stored:?} (bits {:#018x})",
        parsed.to_bits(),
        stored.to_bits(),
    );
}

#[tokio::test]
#[serial]
async fn private_readings_json_arm_is_bit_exact() {
    let (db, app, token) = setup().await;
    insert_fixture_readings(&db).await;

    let (status, body) = crate::common::get_with_token(&app, &readings_uri("json"), &token).await;
    assert_eq!(status, 200, "json readings: {body}");

    #[derive(serde::Deserialize)]
    struct Resp<'a> {
        #[serde(borrow)]
        parameters: Vec<ParamOut<'a>>,
    }
    #[derive(serde::Deserialize)]
    struct ParamOut<'a> {
        #[serde(borrow)]
        values: Vec<Option<&'a RawValue>>,
    }
    let resp: Resp = serde_json::from_str(&body).unwrap_or_else(|e| panic!("parse json: {e}: {body}"));
    let values = &resp.parameters[0].values;
    assert_eq!(values.len(), 3, "three seeded readings: {body}");
    for (cell, stored) in values.iter().zip(fixture_values()) {
        let raw = cell.unwrap_or_else(|| panic!("null cell: {body}"));
        assert_bits_eq(parse_std(raw.get()), stored, "JSON arm");
    }
}

#[tokio::test]
#[serial]
async fn private_readings_csv_arm_is_bit_exact() {
    let (db, app, token) = setup().await;
    insert_fixture_readings(&db).await;

    let (status, body) = crate::common::get_with_token(&app, &readings_uri("csv"), &token).await;
    assert_eq!(status, 200, "csv readings: {body}");

    // Rows: time,<value>. The value column is the only non-time column here (no annotations asked).
    let data: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).skip(1).collect();
    assert_eq!(data.len(), 3, "header + three rows: {body}");
    for (line, stored) in data.iter().zip(fixture_values()) {
        let cell = line.split(',').nth(1).unwrap_or_else(|| panic!("value cell: {line}"));
        assert_bits_eq(parse_std(cell), stored, "CSV arm");
    }
}

#[tokio::test]
#[serial]
async fn private_readings_ndjson_arm_is_bit_exact() {
    let (db, app, token) = setup().await;
    insert_fixture_readings(&db).await;

    let (status, body) = crate::common::get_with_token(&app, &readings_uri("ndjson"), &token).await;
    assert_eq!(status, 200, "ndjson readings: {body}");

    let rows: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(rows.len(), 3, "one object per reading: {body}");
    for (line, stored) in rows.iter().zip(fixture_values()) {
        let obj: std::collections::BTreeMap<String, &RawValue> =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("parse ndjson {line:?}: {e}"));
        let (_, raw) = obj
            .iter()
            .find(|(k, _)| k.as_str() != "time")
            .unwrap_or_else(|| panic!("value field: {line}"));
        assert_bits_eq(parse_std(raw.get()), stored, "NDJSON arm");
    }
}

/// The sensor-vs-grab export builds its number text by hand (`x.to_string()` in a `format!`
/// row), a path distinct from the shared `Table`. A single-replicate grab serves its stored value
/// unchanged (`AVG` over one row), and a single continuous reading in the post-grab window is its
/// own average, so both sides carry a fixture double through to the export.
///
/// Both readings are inserted as raw doubles so the property under test is the export
/// serialization alone: entering the grab through the JSON `/grab_samples` request instead would
/// route the value through the request-body float parser, a separate concern from what the export
/// arm carries out.
#[tokio::test]
#[serial]
async fn sensor_vs_grab_export_is_bit_exact() {
    let (db, app, token) = setup().await;
    // The grab side carries the f32-origin fixture (the ULP-sensitive one); the sensor side an
    // arithmetic result.
    let grab_value = f64::from(100.8_f32);
    let sensor_value = 1683.4228_f64 * 1.3642 + -1.6985;

    // A lone spot reading is its own grab statistic: no sample row, so the export's AVG over one
    // row returns the stored value unchanged.
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings \
                (stream_id, site_id, parameter_id, time, replicate_index, raw_value, \
                 measurement_type) \
             VALUES ('{TEMP_STREAM}', '{site}', '{param}', '2025-06-01T08:00:00Z', 0, \
                     {grab_value:?}, 'spot')",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await;

    // One continuous reading inside [T+2h, T+6h].
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings \
                (stream_id, site_id, parameter_id, time, replicate_index, raw_value, \
                 calibrated_value, measurement_type) \
             VALUES ('{TEMP_STREAM}', '{site}', '{param}', '2025-06-01T12:00:00Z', 0, \
                     {sensor_value:?}, {sensor_value:?}, 'continuous')",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await;

    let uri = format!(
        "/api/sites/{site}/export/sensor-vs-grab?parameter_id={param}\
         &start=2025-06-01T00:00:00Z&end=2025-06-01T23:59:59Z",
        site = crate::common::SITE1_ID,
        param = crate::common::GLOBAL_PARAM_TEMP_ID,
    );

    // JSON arm.
    let (status, json_body) =
        crate::common::get_with_token(&app, &format!("{uri}&format=json"), &token).await;
    assert_eq!(status, 200, "export json: {json_body}");
    #[derive(serde::Deserialize)]
    struct Resp<'a> {
        #[serde(borrow)]
        rows: Vec<Row<'a>>,
    }
    #[derive(serde::Deserialize)]
    struct Row<'a> {
        #[serde(borrow)]
        grab_value: Option<&'a RawValue>,
        #[serde(borrow)]
        sensor_avg: Option<&'a RawValue>,
    }
    let resp: Resp =
        serde_json::from_str(&json_body).unwrap_or_else(|e| panic!("parse export json: {e}"));
    assert_eq!(resp.rows.len(), 1, "one grab, one row: {json_body}");
    let row = &resp.rows[0];
    assert_bits_eq(
        parse_std(row.grab_value.expect("grab_value present").get()),
        grab_value,
        "sensor-vs-grab JSON grab_value",
    );
    assert_bits_eq(
        parse_std(row.sensor_avg.expect("sensor_avg present").get()),
        sensor_value,
        "sensor-vs-grab JSON sensor_avg",
    );

    // CSV arm. Header: time,grab_value,grab_sd,grab_n,sensor_avg,sensor_sd,sensor_n,difference
    let (status, csv) =
        crate::common::get_with_token(&app, &format!("{uri}&format=csv"), &token).await;
    assert_eq!(status, 200, "export csv: {csv}");
    let data = csv
        .lines()
        .filter(|l| !l.trim().is_empty())
        .nth(1)
        .unwrap_or_else(|| panic!("data row: {csv}"));
    let cols: Vec<&str> = data.split(',').collect();
    assert_bits_eq(parse_std(cols[1]), grab_value, "sensor-vs-grab CSV grab_value");
    assert_bits_eq(parse_std(cols[4]), sensor_value, "sensor-vs-grab CSV sensor_avg");
}
