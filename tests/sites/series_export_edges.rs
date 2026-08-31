//! The boundaries of the series export: the empty result, the single row, the cell a parameter
//! has no reading for, the annotation the caller opted into, and a flag reason that contains the
//! delimiter.
//!
//! The governing property is that one query answers with the same rows and the same absences in
//! JSON, CSV and NDJSON, and that `?format` is honoured however few rows come back.
//!
//! Run: cargo test --test sites series_export_edges -- --test-threads=1

use crate::common::db::exec;
use serial_test::serial;

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

fn lines(body: &str) -> Vec<&str> {
    body.lines().filter(|l| !l.trim().is_empty()).collect()
}

fn header(body: &str) -> Vec<String> {
    lines(body)
        .first()
        .map(|l| l.split(',').map(str::to_string).collect())
        .unwrap_or_default()
}

fn column_index(header: &[String], name: &str) -> usize {
    header
        .iter()
        .position(|c| c == name)
        .unwrap_or_else(|| panic!("column {name} missing from header {header:?}"))
}

fn content_type(headers: &axum::http::HeaderMap) -> String {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// Mark one seeded temperature reading as flagged, with a reason carrying the CSV delimiter.
async fn flag_temperature_at(db: &sea_orm::DatabaseConnection, time: &str, reason: &str) {
    exec(
        db,
        &format!(
            "UPDATE readings SET is_flagged = TRUE, flag_reason = '{reason}' \
             WHERE site_id = '{site}' AND parameter_id = '{param}' AND time = '{time}'",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await;
}

#[tokio::test]
#[serial]
async fn a_window_with_no_readings_answers_as_csv_with_a_header_and_no_rows() {
    let (_db, app, token) = setup().await;
    let site = crate::common::SITE1_ID;

    let (status, headers, body) = crate::common::get_with_token_headers(
        &app,
        &format!("/api/sites/{site}/readings?start=2020-01-01T00:00:00Z&end=2020-01-02T00:00:00Z&format=csv"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "empty-window CSV ({status}): {body}");
    assert!(
        content_type(&headers).starts_with("text/csv"),
        "a window with no readings is still CSV, got {}: {body}",
        content_type(&headers)
    );
    assert_eq!(lines(&body).len(), 1, "header only, no data rows: {body}");
    let header = header(&body);
    assert_eq!(
        header.first().map(String::as_str),
        Some("time"),
        "{header:?}"
    );
    assert!(
        header.contains(&"DO_Temperature".to_string()),
        "the site's parameters still name their columns: {header:?}"
    );
}

#[tokio::test]
#[serial]
async fn a_window_with_no_readings_answers_as_ndjson_with_an_empty_body() {
    let (_db, app, token) = setup().await;
    let site = crate::common::SITE1_ID;

    let (status, headers, body) = crate::common::get_with_token_headers(
        &app,
        &format!("/api/sites/{site}/readings?start=2020-01-01T00:00:00Z&end=2020-01-02T00:00:00Z&format=ndjson"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "empty-window NDJSON ({status}): {body}");
    assert!(
        content_type(&headers).starts_with("application/x-ndjson"),
        "got {}: {body}",
        content_type(&headers)
    );
    assert!(
        body.trim().is_empty(),
        "no objects, and no JSON document either: {body}"
    );
}

#[tokio::test]
#[serial]
async fn a_single_timestamp_exports_one_row() {
    let (_db, app, token) = setup().await;
    let site = crate::common::SITE1_ID;

    let (status, body) = crate::common::get_with_token(
        &app,
        &format!(
            "/api/sites/{site}/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T00:00:00Z&format=csv"
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "single-row CSV ({status}): {body}");
    assert_eq!(
        lines(&body).len(),
        2,
        "a header and exactly one row: {body}"
    );
    let header = header(&body);
    let temp = column_index(&header, "DO_Temperature");
    let row: Vec<&str> = lines(&body)[1].split(',').collect();
    assert!(
        row[0].starts_with("2025-01-15T00:00:00"),
        "the row is the requested instant: {body}"
    );
    assert!(
        row[temp].parse::<f64>().is_ok(),
        "and it carries the reading: {body}"
    );
}

#[tokio::test]
#[serial]
async fn a_parameter_with_no_reading_at_a_timestamp_has_an_empty_cell_not_a_zero() {
    let (db, app, token) = setup().await;
    let site = crate::common::SITE1_ID;
    flag_temperature_at(&db, "2025-01-15T00:10:00Z", "instrument out of water").await;

    let window = "start=2025-01-15T00:00:00Z&end=2025-01-15T00:20:00Z&include_flagged=false";

    let (status, body) = crate::common::get_with_token(
        &app,
        &format!("/api/sites/{site}/readings?{window}&format=csv"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "CSV ({status}): {body}");
    let rows = lines(&body);
    assert_eq!(rows.len(), 4, "a header and three timestamps: {body}");
    let header = header(&body);
    let temp = column_index(&header, "DO_Temperature");
    let cond = column_index(&header, "Conductivity");
    let excluded: Vec<&str> = rows[2].split(',').collect();
    assert!(
        excluded[0].starts_with("2025-01-15T00:10:00"),
        "row order: {body}"
    );
    assert_eq!(
        excluded[temp], "",
        "the excluded reading leaves an empty cell, not a fabricated 0: {body}"
    );
    assert!(
        excluded[cond].parse::<f64>().is_ok(),
        "the timestamp survives on the axis because another parameter reported there: {body}"
    );

    let (status, ndjson) = crate::common::get_with_token(
        &app,
        &format!("/api/sites/{site}/readings?{window}&format=ndjson"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "NDJSON ({status}): {ndjson}");
    let objects: Vec<serde_json::Value> = lines(&ndjson)
        .iter()
        .map(|l| serde_json::from_str(l).expect("NDJSON line is JSON"))
        .collect();
    assert_eq!(objects.len(), 3, "one object per timestamp: {ndjson}");
    assert!(
        objects[1]["DO_Temperature"].is_null(),
        "the same absence is null in NDJSON: {ndjson}"
    );

    let (status, json) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site}/readings?{window}"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "JSON ({status}): {json}");
    let block = json["parameters"]
        .as_array()
        .expect("parameters array")
        .iter()
        .find(|p| p["code"] == "DO_Temperature")
        .expect("the temperature block");
    assert!(
        block["values"][1].is_null(),
        "and null in JSON, so the three formats agree: {json}"
    );
}

#[tokio::test]
#[serial]
async fn the_default_export_header_carries_value_columns_only() {
    let (_db, app, token) = setup().await;
    let site = crate::common::SITE1_ID;

    let (status, body) = crate::common::get_with_token(
        &app,
        &format!(
            "/api/sites/{site}/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T00:10:00Z&format=csv"
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "default CSV ({status}): {body}");

    let codes = [
        "Conductivity",
        "DO_Temperature",
        "Depth",
        "Dissolved_O2",
        "Turbidity",
    ];
    let mut expected: Vec<String> = vec!["time".to_string()];
    expected.extend(codes.iter().map(|c| (*c).to_string()));
    expected.sort();

    let mut got = header(&body);
    got.sort();
    assert_eq!(
        got, expected,
        "the default header is the value columns and nothing else, so no opt-in column can \
         arrive without its opt-in: {body}"
    );
}

#[tokio::test]
#[serial]
async fn opting_into_flags_adds_the_columns_and_quotes_a_reason_containing_a_comma() {
    let (db, app, token) = setup().await;
    let site = crate::common::SITE1_ID;
    flag_temperature_at(&db, "2025-01-15T00:10:00Z", "out of water, drifting").await;

    let window = "start=2025-01-15T00:00:00Z&end=2025-01-15T00:20:00Z";
    let (status, body) = crate::common::get_with_token(
        &app,
        &format!("/api/sites/{site}/readings?{window}&include_flags=true&format=csv"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "CSV with flags ({status}): {body}");
    let header = header(&body);
    assert!(
        header.contains(&"DO_Temperature_flagged".to_string()),
        "the opt-in adds the flag column: {header:?}"
    );
    assert!(
        header.contains(&"DO_Temperature_flag_reason".to_string()),
        "and the reason column: {header:?}"
    );
    assert!(
        body.contains("\"out of water, drifting\""),
        "a reason carrying the delimiter is quoted rather than splitting the row: {body}"
    );

    let (status, ndjson) = crate::common::get_with_token(
        &app,
        &format!("/api/sites/{site}/readings?{window}&include_flags=true&format=ndjson"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "NDJSON with flags ({status}): {ndjson}");
    let flagged_object = lines(&ndjson)
        .iter()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("NDJSON line is JSON"))
        .find(|o| o["DO_Temperature_flagged"] == serde_json::Value::Bool(true));
    let flagged_object = flagged_object.unwrap_or_else(|| panic!("no flagged object in {ndjson}"));
    assert_eq!(
        flagged_object["DO_Temperature_flag_reason"].as_str(),
        Some("out of water, drifting"),
        "NDJSON carries the reason verbatim: {ndjson}"
    );
}

#[tokio::test]
#[serial]
async fn the_alarms_opt_in_reaches_the_csv_export() {
    let (_db, app, token) = setup().await;
    let site = crate::common::SITE1_ID;

    let (status, body) = crate::common::get_with_token(
        &app,
        &format!(
            "/api/sites/{site}/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T00:20:00Z&alarms=true&format=csv"
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "CSV with alarms ({status}): {body}");
    let header = header(&body);
    assert!(
        header.contains(&"DO_Temperature_severity".to_string()),
        "the severity the caller asked for is exported: {header:?}"
    );
}

#[tokio::test]
#[serial]
async fn the_aggregates_export_carries_the_flagged_count_and_the_opt_in_severity() {
    let (_db, app, token) = setup().await;
    let site = crate::common::SITE1_ID;
    let window = "start=2025-01-15T00:00:00Z&end=2025-01-15T06:00:00Z";

    let (status, body) = crate::common::get_with_token(
        &app,
        &format!("/api/sites/{site}/aggregates/hourly?{window}&format=csv"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "aggregates CSV ({status}): {body}");
    let columns = header(&body);
    for column in [
        "Depth_avg",
        "Depth_min",
        "Depth_max",
        "Depth_count",
        "Depth_flagged_count",
    ] {
        assert!(
            columns.contains(&column.to_string()),
            "{column} belongs on the aggregates export, matching the JSON body: {columns:?}"
        );
    }
    assert!(
        !columns.iter().any(|c| c.ends_with("_max_severity")),
        "the severity stays off until it is asked for: {columns:?}"
    );

    let (status, with_alarms) = crate::common::get_with_token(
        &app,
        &format!("/api/sites/{site}/aggregates/hourly?{window}&alarms=true&format=csv"),
        &token,
    )
    .await;
    assert_eq!(
        status, 200,
        "aggregates CSV with alarms ({status}): {with_alarms}"
    );
    assert!(
        header(&with_alarms).contains(&"Depth_max_severity".to_string()),
        "and appears when it is: {:?}",
        header(&with_alarms)
    );
}

#[tokio::test]
#[serial]
async fn an_empty_aggregate_window_answers_in_the_requested_format() {
    let (_db, app, token) = setup().await;
    let site = crate::common::SITE1_ID;

    let (status, headers, body) = crate::common::get_with_token_headers(
        &app,
        &format!(
            "/api/sites/{site}/aggregates/daily?start=2020-01-01T00:00:00Z&end=2020-01-05T00:00:00Z&format=csv"
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "empty aggregates CSV ({status}): {body}");
    assert!(
        content_type(&headers).starts_with("text/csv"),
        "got {}: {body}",
        content_type(&headers)
    );
    assert_eq!(lines(&body).len(), 1, "header only: {body}");
}
