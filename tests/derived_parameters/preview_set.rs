//! Scenario: an author previews a formula set before saving it.
//!
//! Expected behaviour: the preview runs the whole set at each instant the site holds the inputs,
//! and answers a series per formula, steps included, so a working number is visible beside the
//! value the calculation publishes.
//!
//! Run with: cargo test --test derived_parameters preview_set

use serde_json::{Value, json};
use serial_test::serial;

const T1: &str = "2025-03-01T00:00:00Z";
const T2: &str = "2025-03-01T00:10:00Z";
const WINDOW_START: &str = "2025-02-28T00:00:00Z";
const WINDOW_END: &str = "2025-03-02T00:00:00Z";

async fn write_reading(
    app: &axum::Router,
    token: &str,
    parameter_id: &str,
    time: &str,
    value: f64,
) {
    let (status, body) = crate::common::post_json_with_token(
        app,
        "/api/readings/batch",
        &json!({
            "readings": [{
                "site_id": crate::common::SITE1_ID,
                "parameter_id": parameter_id,
                "time": time,
                "raw_value": value,
            }]
        }),
        token,
    )
    .await;
    assert!((200..300).contains(&status), "ingest ({status}): {body}");
}

async fn preview(app: &axum::Router, token: &str, formulas: Value) -> Value {
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/actions/preview_derived",
        &json!({
            "formulas": formulas,
            "site_id": crate::common::SITE1_ID,
            "start": WINDOW_START,
            "end": WINDOW_END,
        }),
        token,
    )
    .await;
    assert_eq!(status, 200, "preview ({status}): {body}");
    body
}

fn series<'a>(preview: &'a Value, code: &str) -> &'a Value {
    preview["formulas"]
        .as_array()
        .expect("a series per formula")
        .iter()
        .find(|s| s["code"] == code)
        .unwrap_or_else(|| panic!("no series for {code}: {preview}"))
}

fn at(series: &Value, key: &str, index: usize) -> Value {
    series[key][index].clone()
}

#[tokio::test]
#[serial]
async fn a_set_answers_a_series_per_formula_and_its_steps_feed_the_outputs() {
    let f = crate::common::seeded_app().await;

    write_reading(
        &f.app,
        &f.token,
        crate::common::GLOBAL_PARAM_DO_ID,
        T1,
        250.0,
    )
    .await;
    write_reading(
        &f.app,
        &f.token,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        T1,
        10.0,
    )
    .await;
    write_reading(
        &f.app,
        &f.token,
        crate::common::GLOBAL_PARAM_DO_ID,
        T2,
        500.0,
    )
    .await;
    write_reading(
        &f.app,
        &f.token,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        T2,
        20.0,
    )
    .await;

    let body = preview(
        &f.app,
        &f.token,
        json!([
            { "code": "k_out", "formula": "k_step * 2", "ordinal": 0 },
            { "code": "k_step", "formula": "Dissolved_O2 + 1", "ordinal": 1, "intermediate": true },
            { "code": "k_ratio", "formula": "DO_Temperature / Dissolved_O2", "ordinal": 2 },
        ]),
    )
    .await;

    assert_eq!(body["times"].as_array().expect("times").len(), 2, "{body}");
    let codes: Vec<&str> = body["formulas"]
        .as_array()
        .expect("formulas")
        .iter()
        .map(|s| s["code"].as_str().expect("a code"))
        .collect();
    assert_eq!(codes.len(), 3, "a series per formula: {body}");
    let position = |code: &str| codes.iter().position(|c| *c == code).expect(code);
    assert!(
        position("k_step") < position("k_out"),
        "the set is answered in the order it evaluates, not the order it was written: {codes:?}"
    );

    let step = series(&body, "k_step");
    assert_eq!(step["intermediate"], true, "{step}");
    // 250 + 1, then 500 + 1
    assert_eq!(at(step, "values", 0), json!(251.0), "{step}");
    assert_eq!(at(step, "values", 1), json!(501.0), "{step}");

    let out = series(&body, "k_out");
    assert_eq!(out["intermediate"], false, "{out}");
    // The step's value, not a reading of a parameter named k_step.
    assert_eq!(at(out, "values", 0), json!(502.0), "{out}");
    assert_eq!(at(out, "values", 1), json!(1002.0), "{out}");

    let ratio = series(&body, "k_ratio");
    assert_eq!(at(ratio, "values", 0), json!(0.04), "{ratio}");

    let sources: Vec<&str> = body["source_parameters"]
        .as_array()
        .expect("source_parameters")
        .iter()
        .map(|s| s["name"].as_str().expect("a name"))
        .collect();
    assert!(
        sources.contains(&"Dissolved_O2") && sources.contains(&"DO_Temperature"),
        "the set's stored inputs are answered as series: {body}"
    );
    assert!(
        !sources.contains(&"k_step"),
        "a step is produced by the set, not read from the store: {body}"
    );
}

/// A number the formula could not produce is reported per timestamp, and a value it produced at
/// the timestamp beside it still stands.
#[tokio::test]
#[serial]
async fn a_divide_by_zero_is_an_error_at_that_timestamp_alone() {
    let f = crate::common::seeded_app().await;

    write_reading(
        &f.app,
        &f.token,
        crate::common::GLOBAL_PARAM_DO_ID,
        T1,
        250.0,
    )
    .await;
    write_reading(&f.app, &f.token, crate::common::GLOBAL_PARAM_DO_ID, T2, 0.0).await;

    let body = preview(
        &f.app,
        &f.token,
        json!([{ "code": "k_inv", "formula": "100 / Dissolved_O2", "ordinal": 0 }]),
    )
    .await;

    let inv = series(&body, "k_inv");
    assert_eq!(at(inv, "values", 0), json!(0.4), "{inv}");
    assert_eq!(at(inv, "errors", 0), Value::Null, "{inv}");
    assert_eq!(at(inv, "values", 1), Value::Null, "{inv}");
    assert!(
        at(inv, "errors", 1)
            .as_str()
            .is_some_and(|e| e.contains("finite")),
        "the divide by zero says so: {inv}"
    );
}

/// A formula naming something the catalog does not hold is refused, as it is at the save.
#[tokio::test]
#[serial]
async fn an_unknown_identifier_is_refused() {
    let f = crate::common::seeded_app().await;

    let (status, body) = crate::common::post_json_with_token(
        &f.app,
        "/api/actions/preview_derived",
        &json!({
            "formulas": [{ "code": "k", "formula": "not_a_thing * 2", "ordinal": 0 }],
            "site_id": crate::common::SITE1_ID,
            "start": WINDOW_START,
            "end": WINDOW_END,
        }),
        &f.token,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("not_a_thing"), "the refusal names it: {body}");
}
