//! The public readings arms carry the stored double unchanged for a slot with no declared
//! precision. Q14 amends Q31 for the public API: a slot that declares `decimal_places` expresses
//! its values at those places, but a slot with no declaration is served unrounded, exactly as the
//! private arms are. This pins that undeclared-slot contract as bit-for-bit round-trip across the
//! public JSON, CSV and NDJSON arms.
//!
//! The declared-places arm of Q14 (a declared slot rounding on the way out) is not yet built: the
//! public readings path serves `COALESCE(calibrated, raw)` unrounded regardless of the slot's
//! `decimal_places`, so this suite uses an undeclared slot, the half of the Q14 contract the code
//! satisfies today.
//!
//! Emitted number text is captured with `RawValue` and parsed with the standard-library `f64`
//! parser, not `serde_json::Value::as_f64` (off by one ULP on the f32-origin fixture).
//!
//! Run: cargo test --test public_api export_double_roundtrip -- --test-threads=1

use serde_json::value::RawValue;
use serial_test::serial;

fn fixture_values() -> [f64; 3] {
    [0.1_f64 + 0.2, 1683.4228_f64 * 1.3642 + -1.6985, f64::from(100.8_f32)]
}

const T0: &str = "2025-06-01T00:00:00Z";
const T1: &str = "2025-06-01T00:10:00Z";
const T2: &str = "2025-06-01T00:20:00Z";
const WINDOW: &str = "start=2025-06-01T00:00:00Z&end=2025-06-01T01:00:00Z";
const READINGS_URI: &str = "/api/public/test-river/sites/upstream/readings";

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    crate::common::exec(
        &db,
        &format!(
            "UPDATE projects SET is_public = true, public_code = 'test-river' WHERE id = '{}'",
            crate::common::PROJECT_ID
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE sites SET public_code = 'upstream' WHERE id = '{}'",
            crate::common::SITE1_ID
        ),
    )
    .await;
    // Undeclared precision: this is the half of the Q14 contract the public arm satisfies today.
    crate::common::exec(
        &db,
        &format!(
            "UPDATE site_parameters SET is_public = true, decimal_places = NULL WHERE id = '{}'",
            crate::common::PARAM_S1_TEMP_ID
        ),
    )
    .await;

    let [v0, v1, v2] = fixture_values();
    for (time, value) in [(T0, v0), (T1, v1), (T2, v2)] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO readings \
                    (stream_id, site_id, parameter_id, time, replicate_index, raw_value, \
                     measurement_type) \
                 VALUES ('00000000-0000-4000-d000-000000000001', '{site}', '{param}', '{time}', 0, \
                         {value:?}, 'continuous')",
                site = crate::common::SITE1_ID,
                param = crate::common::GLOBAL_PARAM_TEMP_ID,
            ),
        )
        .await;
    }

    let app = crate::common::build_test_app(db.clone());
    (db, app)
}

fn parse_std(text: &str) -> f64 {
    text.trim()
        .parse()
        .unwrap_or_else(|e| panic!("parse f64 from {text:?}: {e}"))
}

fn assert_bits_eq(parsed: f64, stored: f64, context: &str) {
    assert_eq!(
        parsed.to_bits(),
        stored.to_bits(),
        "{context}: exported {parsed:?} != stored {stored:?}",
    );
}

#[tokio::test]
#[serial]
async fn public_readings_json_arm_is_bit_exact_for_undeclared_slot() {
    let (_db, app) = setup().await;
    let (status, body) = crate::common::get(&app, &format!("{READINGS_URI}?{WINDOW}")).await;
    assert_eq!(status, 200, "public json: {body}");

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
    let resp: Resp = serde_json::from_str(&body).unwrap_or_else(|e| panic!("parse: {e}: {body}"));
    let values = &resp.parameters[0].values;
    assert_eq!(values.len(), 3, "three readings: {body}");
    for (cell, stored) in values.iter().zip(fixture_values()) {
        let raw = cell.unwrap_or_else(|| panic!("null cell: {body}"));
        assert_bits_eq(parse_std(raw.get()), stored, "public JSON arm");
    }
}

#[tokio::test]
#[serial]
async fn public_readings_csv_arm_is_bit_exact_for_undeclared_slot() {
    let (_db, app) = setup().await;
    let (status, body) =
        crate::common::get(&app, &format!("{READINGS_URI}?{WINDOW}&format=csv")).await;
    assert_eq!(status, 200, "public csv: {body}");

    let data: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).skip(1).collect();
    assert_eq!(data.len(), 3, "header + three rows: {body}");
    for (line, stored) in data.iter().zip(fixture_values()) {
        let cell = line.split(',').nth(1).unwrap_or_else(|| panic!("value cell: {line}"));
        assert_bits_eq(parse_std(cell), stored, "public CSV arm");
    }
}

#[tokio::test]
#[serial]
async fn public_readings_ndjson_arm_is_bit_exact_for_undeclared_slot() {
    let (_db, app) = setup().await;
    let (status, body) =
        crate::common::get(&app, &format!("{READINGS_URI}?{WINDOW}&format=ndjson")).await;
    assert_eq!(status, 200, "public ndjson: {body}");

    let rows: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(rows.len(), 3, "one object per reading: {body}");
    for (line, stored) in rows.iter().zip(fixture_values()) {
        let obj: std::collections::BTreeMap<String, &RawValue> =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("parse {line:?}: {e}"));
        let (_, raw) = obj
            .iter()
            .find(|(k, _)| k.as_str() != "time")
            .unwrap_or_else(|| panic!("value field: {line}"));
        assert_bits_eq(parse_std(raw.get()), stored, "public NDJSON arm");
    }
}
