//! Tests for alarm edge cases: duplicate threshold JOIN, NULL thresholds.
//!
//! Run with: cargo test --test alarm_edge_cases_test
//! Requires: DATABASE_URL pointing to a TimescaleDB instance.

mod common;

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

// ============================================================================
// Duplicate threshold: both global + site-specific for the same parameter
// ============================================================================

#[tokio::test]
#[serial]
async fn test_global_plus_site_specific_threshold_no_duplicate_rows() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());

    let site_id = common::SITE1_ID;

    // The seed data already has a GLOBAL threshold (site_id IS NULL) for DO_Temperature.
    // Now add a SITE-SPECIFIC threshold for the same parameter at site 1.
    // This creates the conditions for the duplicate JOIN bug.
    exec(
        &db,
        &format!(
            "INSERT INTO alarm_thresholds (id, parameter_id, site_id, warning_min, warning_max, alarm_min, alarm_max, description) \
             VALUES (gen_random_uuid(), '{param_id}', '{site_id}', 1.0, 18.0, 0.0, 22.0, 'Site-specific temp threshold')",
            param_id = common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await;

    // Query alarms for a narrow time range that contains step 50 (warning-level temp value = 22°C)
    // Step 50 = base_time + 50*10min = 2025-01-15T08:20:00Z
    let (status, body) = common::get_json_with_token(
        &app,
        &format!(
            "/api/sites/{site_id}/alarms?start=2025-01-15T08:00:00Z&end=2025-01-15T09:00:00Z"
        ),
        &token,
    )
    .await;

    assert_eq!(status, 200);

    let times = body["times"].as_array().unwrap();
    let params = body["parameters"].as_array().unwrap();

    // Count how many violation entries exist for DO_Temperature
    let temp_param = params
        .iter()
        .find(|p| p["type"].as_str() == Some("DO_Temperature"));

    if let Some(temp) = temp_param {
        let severities = temp["severities"].as_array().unwrap();

        // Each timestamp should appear at most ONCE per parameter.
        // Before the fix, the JOIN would produce duplicate rows.
        // Count non-zero severities
        let violation_count: usize = severities
            .iter()
            .filter(|s| s.as_i64().unwrap_or(0) > 0)
            .count();

        // In this 1-hour range, there should be at most ~6 readings (10-min intervals).
        // With step 50 at 08:20, we might see 1 violation.
        // The key assertion: violations should not exceed the number of unique timestamps.
        assert!(
            violation_count <= times.len(),
            "violation count ({violation_count}) should not exceed unique timestamp count ({}). \
             Duplicate rows from JOIN indicate the bug is present.",
            times.len()
        );
    }

    // Also verify that each (parameter, time) pair appears exactly once:
    // The times array should have no duplicates
    let mut times_sorted: Vec<&str> = times
        .iter()
        .filter_map(|t| t.as_str())
        .collect();
    let original_len = times_sorted.len();
    times_sorted.sort();
    times_sorted.dedup();
    assert_eq!(
        times_sorted.len(),
        original_len,
        "times array should have no duplicates — indicates JOIN produced duplicate rows"
    );

    common::cleanup_test_db(&db).await;
}

// ============================================================================
// All NULL thresholds (no violations expected)
// ============================================================================

#[tokio::test]
#[serial]
async fn test_all_null_thresholds_no_violations() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());

    let site_id = common::SITE1_ID;

    // Remove all thresholds and replace with one that has all NULL bounds
    exec(&db, "DELETE FROM alarm_thresholds").await;
    exec(
        &db,
        &format!(
            "INSERT INTO alarm_thresholds (id, parameter_id, description) \
             VALUES (gen_random_uuid(), '{param_id}', 'All NULL threshold')",
            param_id = common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await;

    let (status, body) = common::get_json_with_token(
        &app,
        &format!(
            "/api/sites/{site_id}/alarms?start=2025-01-15T00:00:00Z&end=2025-01-17T00:00:00Z"
        ),
        &token,
    )
    .await;

    assert_eq!(status, 200);

    let times = body["times"].as_array().unwrap();
    assert!(
        times.is_empty(),
        "all-NULL thresholds should produce no violations, got {} times",
        times.len()
    );
}
