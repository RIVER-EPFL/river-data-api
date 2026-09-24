//! The period statistics table the portals rendered under every time series.
//!
//! Time Points, N, NA's, Median, Mean, SD, Min and Max per parameter over the displayed range,
//! computed in SQL over the values the API serves. The SD is the sample sd, R's `sd()`.
//!
//! Run: cargo test --test sites period_statistics -- --test-threads=1

use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_DO_ID, SITE1_ID};

/// 1, 2 and 10 at three instants: median 2, mean 13/3, sample sd sqrt(146/3/2).
const VALUES: [(f64, &str); 3] = [
    (1.0, "2025-04-01T06:00:00Z"),
    (2.0, "2025-04-02T06:00:00Z"),
    (10.0, "2025-04-03T06:00:00Z"),
];

fn close(actual: Option<f64>, expected: f64, what: &str) {
    let actual = actual.unwrap_or_else(|| panic!("{what} is served"));
    assert!(
        (actual - expected).abs() < 1e-9,
        "{what}: {actual} is not {expected}"
    );
}

async fn seed_continuous(db: &sea_orm::DatabaseConnection) {
    let stream_id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active) \
             VALUES ('{stream_id}', 'period-stats', '{}', true)",
            Uuid::new_v4()
        ),
    )
    .await;
    for (value, time) in VALUES {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                    raw_value, measurement_type) \
                 VALUES ('{stream_id}', '{SITE1_ID}', '{GLOBAL_PARAM_DO_ID}', '{time}', 0, \
                         {value}, 'continuous')"
            ),
        )
        .await;
    }
}

async fn statistics(app: &axum::Router, token: &str, extra: &str) -> serde_json::Value {
    let (status, body) = crate::common::get_json_with_token(
        app,
        &format!(
            "/api/sites/{SITE1_ID}/statistics?start=2025-03-25T00:00:00Z&end=2025-04-10T00:00:00Z\
             &parameter_ids={GLOBAL_PARAM_DO_ID}{extra}"
        ),
        token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "statistics ({status}): {body}"
    );
    body
}

/// Expected behaviour: the portal's eight rows, over the continuous series, with the sample sd.
#[tokio::test]
#[serial]
async fn a_continuous_period_reports_the_portal_s_rows_with_the_sample_sd() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    seed_continuous(&db).await;

    let body = statistics(&app, &token, "").await;
    let row = &body["parameters"][0];
    assert_eq!(row["time_points"], 3, "{row}");
    assert_eq!(row["n"], 3, "{row}");
    assert_eq!(row["nulls"], 0, "{row}");
    close(row["median"].as_f64(), 2.0, "median");
    close(row["mean"].as_f64(), 13.0 / 3.0, "mean");
    close(row["min"].as_f64(), 1.0, "min");
    close(row["max"].as_f64(), 10.0, "max");
    close(
        row["stdev_sample"].as_f64(),
        (146.0f64 / 3.0 / 2.0).sqrt(),
        "the n-1 standard deviation",
    );
    assert!(row.get("stdev_population").is_none(), "{row}");
    assert!(row["units"].is_string(), "the rows name their unit: {row}");
}

/// Expected behaviour: a spot period summarises the served instant values, so a triplicate visit
/// contributes one number rather than three.
#[tokio::test]
#[serial]
async fn a_spot_period_summarises_instants_not_replicates() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, body) = crate::common::post_checked_grab(
        &app,
        &json!({
            "site_id": SITE1_ID,
            "readings": [
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 1.0, "time": VALUES[0].1 },
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 3.0, "time": VALUES[0].1 },
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 8.0, "time": VALUES[1].1 },
            ],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let body = statistics(&app, &token, "&measurement_type=spot").await;
    let row = &body["parameters"][0];
    assert_eq!(row["n"], 2, "two visits, not three replicates: {row}");
    close(row["mean"].as_f64(), 5.0, "the mean of the served instants");
    close(
        row["min"].as_f64(),
        2.0,
        "the triplicate's own mean is the low value",
    );
}

/// Expected behaviour: an unknown cadence is refused rather than quietly summarising the other one.
#[tokio::test]
#[serial]
async fn an_unknown_cadence_is_refused() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, _body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{SITE1_ID}/statistics?measurement_type=hourly"),
        &token,
    )
    .await;
    assert_eq!(
        status, 400,
        "a cadence that does not exist is a bad request"
    );
}

/// Expected behaviour: an export carries the sample sd and the other statistics, and no divisor
/// column, because there is one divisor.
#[tokio::test]
#[serial]
async fn an_export_carries_the_statistics_without_a_divisor_column() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, body) = crate::common::post_checked_grab(
        &app,
        &json!({
            "site_id": SITE1_ID,
            "readings": [
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 1.0, "time": VALUES[0].1 },
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 3.0, "time": VALUES[0].1 },
            ],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let (status, csv) = crate::common::get_with_token(
        &app,
        &format!(
            "/api/sites/{SITE1_ID}/readings?start=2025-03-25T00:00:00Z&end=2025-04-10T00:00:00Z\
             &measurement_type=spot&include_sample_stats=true&format=csv"
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{csv}");

    let header = csv.lines().next().expect("a header row");
    assert!(
        header.contains("_sd"),
        "the sd travels with the mean: {header}"
    );
    assert!(!header.contains("_sd_estimator"), "{header}");
    assert!(
        header.contains("_median"),
        "the median travels with the other statistics: {header}"
    );
}
