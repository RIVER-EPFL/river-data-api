//! The seasonal Check gate: pooled replicates, ±2-month cyclic window across all years,
//! min/Q10/Q90/max classification with reachable extremes, and the save-side validation that an
//! edit after a check needs a fresh check.
//!
//! Run: cargo test --test readings seasonal_check -- --test-threads=1

use sea_orm::DatabaseConnection;
use serde_json::json;
use serial_test::serial;

use crate::common::{GLOBAL_PARAM_DO_ID, SITE1_ID};

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

/// Grab replicate history: `values` replicates at one instant.
async fn seed_visit(app: &axum::Router, token: &str, at: &str, values: &[f64]) {
    let readings: Vec<serde_json::Value> = values
        .iter()
        .map(|v| json!({ "parameter_id": GLOBAL_PARAM_DO_ID, "value": v, "time": at }))
        .collect();
    let (status, body) = crate::common::post_json_with_token(
        app,
        "/api/grab_samples",
        &json!({ "site_id": SITE1_ID, "readings": readings }),
        token,
    )
    .await;
    assert_eq!(status, 200, "seed visit at {at}: {body}");
}

async fn check(
    app: &axum::Router,
    token: &str,
    time: &str,
    value: f64,
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        app,
        "/api/readings/seasonal_check",
        &json!({
            "site_id": SITE1_ID,
            "time": time,
            "values": [{ "parameter_id": GLOBAL_PARAM_DO_ID, "value": value }],
        }),
        token,
    )
    .await
}

/// June entries across three years, replicates pooled; December carries wild values that must not
/// reach a June window.
async fn seed_history(app: &axum::Router, token: &str) {
    for (at, values) in [
        ("2021-06-10T10:00:00Z", &[9.0, 10.0, 11.0][..]),
        ("2022-05-20T10:00:00Z", &[10.0, 12.0][..]),
        ("2023-07-05T10:00:00Z", &[8.0, 9.5][..]),
        ("2023-12-15T10:00:00Z", &[1000.0, 1200.0][..]),
    ] {
        seed_visit(app, token, at, values).await;
    }
}

#[tokio::test]
#[serial]
async fn the_window_pools_replicates_across_years_and_stays_seasonal() {
    let (_db, app, token) = setup().await;
    seed_history(&app, &token).await;

    let (status, resp) = check(&app, &token, "2025-06-15T10:00:00Z", 10.5).await;
    assert_eq!(status, 200, "{resp}");
    let f = &resp["findings"][0];
    assert_eq!(f["n"], 7, "May–July replicates across all years, December excluded: {resp}");
    assert_eq!(f["class"], "normal");
    assert_eq!(f["min"], 8.0);
    assert_eq!(f["max"], 12.0);
    assert_eq!(resp["warnings"], 0);
    assert_eq!(
        f["distribution"].as_array().map(Vec::len),
        Some(7),
        "the distribution payload carries the pooled values: {resp}"
    );
}

#[tokio::test]
#[serial]
async fn a_value_beyond_the_recorded_range_reports_beyond_it() {
    let (_db, app, token) = setup().await;
    seed_history(&app, &token).await;

    // The portal's unreachable 'max' label: > max must not report as merely above Q90.
    let (status, resp) = check(&app, &token, "2025-06-15T10:00:00Z", 55.0).await;
    assert_eq!(status, 200, "{resp}");
    assert_eq!(resp["findings"][0]["class"], "above_max", "{resp}");
    assert_eq!(resp["findings"][0]["warning"], true);

    let (_, resp) = check(&app, &token, "2025-06-15T10:00:00Z", 0.5).await;
    assert_eq!(resp["findings"][0]["class"], "below_min", "{resp}");

    // No history at all is its own answer, not a warning. (Every month is within two of some
    // seeded DO visit, so the empty case is a parameter with no grabs.)
    let (_, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/seasonal_check",
        &json!({
            "site_id": SITE1_ID,
            "time": "2025-06-15T10:00:00Z",
            "values": [{ "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID, "value": 10.0 }],
        }),
        &token,
    )
    .await;
    assert_eq!(resp["findings"][0]["class"], "no_history", "{resp}");
    assert_eq!(resp["findings"][0]["warning"], false);
}

#[tokio::test]
#[serial]
async fn the_month_window_wraps_the_year_boundary() {
    let (_db, app, token) = setup().await;
    seed_history(&app, &token).await;

    // A January entry reaches December (distance 1) but not June (distance 5 or 7).
    let (status, resp) = check(&app, &token, "2025-01-10T10:00:00Z", 1100.0).await;
    assert_eq!(status, 200, "{resp}");
    let f = &resp["findings"][0];
    assert_eq!(f["n"], 2, "only the December visit is in a January window: {resp}");
    assert_eq!(f["class"], "normal", "1100 sits inside the December range: {resp}");
}

#[tokio::test]
#[serial]
async fn a_save_is_held_to_the_check_it_names() {
    let (_db, app, token) = setup().await;
    seed_history(&app, &token).await;

    let (status, resp) = check(&app, &token, "2025-06-15T10:00:00Z", 10.2).await;
    assert_eq!(status, 200, "{resp}");
    let check_id = resp["check_id"].as_str().expect("check id").to_string();

    // Values edited after the check: refused, the portal's reset rule server-side.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "check_id": check_id,
            "readings": [{ "parameter_id": GLOBAL_PARAM_DO_ID, "value": 10.3, "time": "2025-06-15T10:00:00Z" }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 409, "an edited value needs a fresh check: {body}");
    assert!(body.contains("fresh check"), "{body}");

    // The checked value saves.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "check_id": check_id,
            "readings": [{ "parameter_id": GLOBAL_PARAM_DO_ID, "value": 10.2, "time": "2025-06-15T10:00:00Z" }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // A check for another site cannot vouch for this one.
    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/seasonal_check",
        &json!({
            "site_id": crate::common::SITE2_ID,
            "time": "2025-06-15T11:00:00Z",
            "values": [{ "parameter_id": GLOBAL_PARAM_DO_ID, "value": 7.0 }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{resp}");
    let foreign_check = resp["check_id"].as_str().unwrap();
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "check_id": foreign_check,
            "readings": [{ "parameter_id": GLOBAL_PARAM_DO_ID, "value": 7.0, "time": "2025-06-15T11:00:00Z" }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "a foreign check is refused: {body}");
    assert!(body.contains("different site"), "{body}");
}
