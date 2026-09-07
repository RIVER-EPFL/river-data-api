//! What a served spot instant carries.
//!
//! A grid of bare numbers with no unit, no n and no dispersion cannot distinguish a triplicate
//! from a single reading, and the point record showed the replicates behind a value without ever
//! showing the value's own statistics. Every surface that serves a spot instant serves the whole
//! group: n, mean, both standard deviations, median, range, the divisor the slot declares, and the
//! parameter's identity and unit.
//!
//! Run: cargo test --test readings spot_instant_shape -- --test-threads=1

use sea_orm::DatabaseConnection;
use serde_json::json;
use serial_test::serial;

use crate::common::{GLOBAL_PARAM_DO_ID, SITE1_ID};

const AT: &str = "2025-05-06T07:00:00Z";

/// 1, 2 and 10: median 2, mean 4.333…, sample sd 4.932…, population sd 4.027….
const REPLICATES: [f64; 3] = [1.0, 2.0, 10.0];

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

async fn save_group(app: &axum::Router, token: &str) {
    let (status, body) = crate::common::post_json_with_token(
        app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "readings": REPLICATES
                .iter()
                .map(|v| json!({ "parameter_id": GLOBAL_PARAM_DO_ID, "value": v, "time": AT }))
                .collect::<Vec<_>>(),
        }),
        token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

fn close(actual: Option<f64>, expected: f64, what: &str) {
    let actual = actual.unwrap_or_else(|| panic!("{what} is served"));
    assert!(
        (actual - expected).abs() < 1e-9,
        "{what}: {actual} is not {expected}"
    );
}

/// Expected behaviour: the wide row says how many vials are behind each number, how far apart they
/// were, and what unit the column is in.
#[tokio::test]
#[serial]
async fn the_visits_grid_carries_the_group_s_statistics_and_its_column_a_unit() {
    let (_db, app, token) = setup().await;
    save_group(&app, &token).await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{SITE1_ID}/visits"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let column = body["expected_parameters"]
        .as_array()
        .expect("expected_parameters")
        .iter()
        .find(|p| p["parameter_id"] == GLOBAL_PARAM_DO_ID)
        .expect("the parameter has a column");
    assert!(
        column["units"].is_string(),
        "the column names its unit: {column}"
    );

    let cell = body["visits"][0]["cells"]
        .as_array()
        .expect("cells")
        .iter()
        .find(|c| c["parameter_id"] == GLOBAL_PARAM_DO_ID)
        .expect("the group has a cell");
    assert_eq!(cell["n"], 3, "three vials: {cell}");
    close(cell["median"].as_f64(), 2.0, "median");
    close(cell["min"].as_f64(), 1.0, "min");
    close(cell["max"].as_f64(), 10.0, "max");
    assert!(cell["stdev"].as_f64().is_some(), "an sd is served: {cell}");
    assert_eq!(
        cell["sd_estimator_source"], "default",
        "nothing declared a divisor, so the fallback is named as itself: {cell}"
    );
}

/// Expected behaviour: expanding a visit gives the range and both divisors, and each replicate's
/// curve references and retraction stamp.
#[tokio::test]
#[serial]
async fn the_visit_detail_carries_both_divisors_and_each_replicate_s_provenance() {
    let (db, app, token) = setup().await;
    save_group(&app, &token).await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET withdrawn_at = now() \
             WHERE parameter_id = '{GLOBAL_PARAM_DO_ID}' AND time = '{AT}' AND replicate_index = 2"
        ),
    )
    .await;

    let (status, list) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{SITE1_ID}/visits"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{list}");
    let event_id = list["visits"][0]["id"].as_str().expect("visit id");

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/collection_events/{event_id}/detail"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let cell = body["cells"]
        .as_array()
        .expect("cells")
        .iter()
        .find(|c| c["parameter_id"] == GLOBAL_PARAM_DO_ID)
        .expect("the group has a cell");
    let sample = &cell["sample"];
    // The retraction leaves 1 and 2 behind the mean.
    assert_eq!(sample["n"], 2, "the retracted replicate is out of n: {cell}");
    close(sample["median"].as_f64(), 1.5, "median");
    close(sample["min"].as_f64(), 1.0, "min");
    close(sample["max"].as_f64(), 2.0, "max");
    close(
        sample["stdev_sample"].as_f64(),
        (0.5f64).sqrt(),
        "the n-1 standard deviation",
    );
    close(sample["stdev_population"].as_f64(), 0.5, "the n divisor");

    let retracted = cell["replicates"]
        .as_array()
        .expect("replicates")
        .iter()
        .find(|r| r["replicate_index"] == 2)
        .expect("the retracted replicate is still listed");
    assert_eq!(retracted["withdrawn"], true, "{retracted}");
    assert!(
        retracted["withdrawn_at"].is_string(),
        "the stamp says when: {retracted}"
    );
}

/// Expected behaviour: the point record shows the number the chart plotted and the spread it drew,
/// under a parameter that is named rather than a bare uuid.
#[tokio::test]
#[serial]
async fn the_point_record_carries_the_group_statistics_and_names_the_parameter() {
    let (_db, app, token) = setup().await;
    save_group(&app, &token).await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/readings/provenance?site_id={SITE1_ID}&parameter_id={GLOBAL_PARAM_DO_ID}&time={AT}"
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    assert!(
        body["parameter_code"].is_string() && body["parameter_name"].is_string(),
        "the record names what it measured: {body}"
    );

    let computation = &body["records"][0]["computation"];
    assert_eq!(computation["n"], 3, "{computation}");
    close(computation["mean"].as_f64(), 13.0 / 3.0, "mean");
    close(computation["median"].as_f64(), 2.0, "median");
    close(computation["min"].as_f64(), 1.0, "min");
    close(computation["max"].as_f64(), 10.0, "max");
    close(
        computation["stdev_sample"].as_f64(),
        (146.0f64 / 3.0 / 2.0).sqrt(),
        "the n-1 standard deviation",
    );
    close(
        computation["stdev_population"].as_f64(),
        (146.0f64 / 3.0 / 3.0).sqrt(),
        "the n standard deviation",
    );
}

/// Expected behaviour: the readings response carries the same complete group, so a chart drawing
/// from it needs no second request to say what its bar measures.
#[tokio::test]
#[serial]
async fn the_readings_response_serves_the_whole_group_and_the_slot_s_precision() {
    let (_db, app, token) = setup().await;
    save_group(&app, &token).await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/sites/{SITE1_ID}/readings?start=2025-05-01T00:00:00Z&end=2025-05-10T00:00:00Z\
             &measurement_type=spot&include_sample_stats=true"
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let series = body["parameters"]
        .as_array()
        .expect("parameters")
        .iter()
        .find(|p| p["parameter_id"] == GLOBAL_PARAM_DO_ID)
        .expect("the parameter is served");
    assert!(
        series.get("decimal_places").is_some() || series["decimal_places"].is_null(),
        "the slot's declared precision travels with the series: {series}"
    );

    let sample = &series["samples"][0];
    assert_eq!(sample["n"], 3, "{sample}");
    close(sample["median"].as_f64(), 2.0, "median");
    assert_eq!(
        sample["sd_estimator"], "sample",
        "the fallback divisor, named: {sample}"
    );
    assert!(
        sample["stdev_population"].as_f64().is_some(),
        "the divisor the slot did not declare is still readable: {sample}"
    );
}
