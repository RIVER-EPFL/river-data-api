//! Tests for status events endpoint, particularly CSV quoting.
//!
//! Run with: cargo test --test status_events
//! Requires: DATABASE_URL pointing to a TimescaleDB instance.


use serial_test::serial;

// ============================================================================
// Helper
// ============================================================================

async fn exec(db: &sea_orm::DatabaseConnection, sql: &str) {
    use sea_orm::{ConnectionTrait, Statement};
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("SQL failed: {e}\nQuery: {sql}"));
}

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

// ============================================================================
// CSV quoting: value with comma
// ============================================================================

#[tokio::test]
#[serial]
async fn test_csv_value_with_comma_is_properly_quoted() {
    let (db, app, token) = setup().await;

    let site_id = crate::common::SITE1_ID;
    let param_id = crate::common::GLOBAL_PARAM_TEMP_ID;
    let stream_id = "00000000-0000-4000-d000-000000000001";

    exec(
        &db,
        &format!(
            "INSERT INTO status_events (stream_id, site_id, parameter_id, time, value) \
             VALUES ('{stream_id}', '{site_id}', '{param_id}', '2025-01-15T06:00:00Z', 'running,ok')"
        ),
    )
    .await;

    // Request CSV format
    let (status, body) = crate::common::get_with_token(
        &app,
        &format!(
            "/api/sites/{site_id}/status_events?start=2025-01-15T00:00:00Z&end=2025-01-16T00:00:00Z&format=csv"
        ),
        &token,
    )
    .await;

    assert_eq!(status, 200);

    // Parse the CSV body
    let lines: Vec<&str> = body.lines().collect();
    assert!(lines.len() >= 2, "should have header + at least 1 data row");

    // Header should have 4 columns
    let header_cols: Vec<&str> = lines[0].split(',').collect();
    assert_eq!(
        header_cols.len(),
        4,
        "header should have 4 columns: time,parameter_id,value,sensor_id"
    );

    // Find the data row with "running,ok" — it should be properly quoted
    // If the value is NOT quoted, parsing would see 5 columns instead of 4
    let data_line = lines
        .iter()
        .skip(1)
        .find(|l| l.contains("running"));

    assert!(data_line.is_some(), "should find a row with 'running' value");

    let line = data_line.unwrap();
    // Use the csv crate to parse — it handles RFC 4180 quoting
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .from_reader(line.as_bytes());
    let record = rdr.records().next().unwrap().unwrap();

    assert_eq!(
        record.len(),
        4,
        "CSV row with comma-containing value should still parse as 4 fields, got {}: {:?}",
        record.len(),
        record,
    );
    assert_eq!(
        record.get(2).unwrap(),
        "running,ok",
        "value field should contain the full string including comma"
    );
}

// ============================================================================
// Empty result
// ============================================================================

#[tokio::test]
#[serial]
async fn test_status_events_empty_range() {
    let (_db, app, token) = setup().await;

    let site_id = crate::common::SITE1_ID;

    // Query a time range with no status events
    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/sites/{site_id}/status_events?start=2020-01-01T00:00:00Z&end=2020-01-02T00:00:00Z"
        ),
        &token,
    )
    .await;

    assert_eq!(status, 200);

    let events = body["events"].as_array().unwrap();
    assert!(events.is_empty(), "should return empty events array");
    assert_eq!(body["total"], 0);
}

// ============================================================================
// NDJSON format
// ============================================================================

#[tokio::test]
#[serial]
async fn test_status_events_ndjson_format() {
    let (db, app, token) = setup().await;

    let site_id = crate::common::SITE1_ID;
    let param_id = crate::common::GLOBAL_PARAM_TEMP_ID;
    let stream_id = "00000000-0000-4000-d000-000000000001";

    exec(
        &db,
        &format!(
            "INSERT INTO status_events (stream_id, site_id, parameter_id, time, value) \
             VALUES ('{stream_id}', '{site_id}', '{param_id}', '2025-01-15T06:00:00Z', 'online')"
        ),
    )
    .await;

    let (status, body) = crate::common::get_with_token(
        &app,
        &format!(
            "/api/sites/{site_id}/status_events?start=2025-01-15T00:00:00Z&end=2025-01-16T00:00:00Z&format=ndjson"
        ),
        &token,
    )
    .await;

    assert_eq!(status, 200);

    // Each line should be valid JSON
    for line in body.lines() {
        if line.is_empty() {
            continue;
        }
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(line);
        assert!(
            parsed.is_ok(),
            "each NDJSON line should be valid JSON, got error: {:?} for line: {line}",
            parsed.err()
        );
    }
}
