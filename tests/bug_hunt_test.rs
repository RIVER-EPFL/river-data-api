//! Bug-hunting tests: each test targets a specific suspected defect.
//! These tests are EXPECTED TO FAIL initially, exposing real bugs.
//! After fixing the bug, the test becomes a regression guard.

mod common;

use serial_test::serial;

// ============================================================================
// Bug 1: Flagged readings still included in continuous aggregates
//
// The aggregate view definition is:
//   WHERE site_id IS NOT NULL AND replicate_index = 0
// but does NOT include: AND (is_flagged IS NOT TRUE)
// So flagged outliers are averaged into aggregates.
// ============================================================================

#[tokio::test]
#[serial]
async fn bug1_flagged_readings_still_in_aggregates() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;

    // Seed minimal data: 1 project, 1 site, 1 parameter, 1 site_parameter
    common::db::exec(
        &db,
        &format!(
            "INSERT INTO projects (id, name, data_source) VALUES ('{pid}', 'Bug1 Project', 'test')",
            pid = common::PROJECT_ID
        ),
    )
    .await;
    common::db::exec(
        &db,
        &format!(
            "INSERT INTO sites (id, project_id, name) VALUES ('{sid}', '{pid}', 'Bug1 Site')",
            sid = common::SITE1_ID,
            pid = common::PROJECT_ID
        ),
    )
    .await;
    common::db::exec(
        &db,
        &format!(
            "INSERT INTO parameters (id, name, display_name, default_units, category, data_type) \
             VALUES ('{gid}', 'Temperature', 'Temperature', '°C', 'measurement', 'numeric')",
            gid = common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    common::db::exec(
        &db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, display_units, sample_interval_sec, is_active) \
             VALUES ('{spid}', '{sid}', '{gid}', 'Temperature', '°C', 600, true)",
            spid = common::PARAM_S1_TEMP_ID,
            sid = common::SITE1_ID,
            gid = common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;

    // Create a data stream (readings require stream_id)
    common::seed_data_stream(&db, common::STREAM1_ID, "test", "bug1_stream").await;
    // Pair it to the site_parameter
    common::db::exec(
        &db,
        &format!(
            "UPDATE data_streams SET site_parameter_id = '{spid}' WHERE id = '{sid}'",
            spid = common::PARAM_S1_TEMP_ID,
            sid = common::STREAM1_ID
        ),
    )
    .await;

    // Insert 5 readings in the same hour bucket: [10, 11, 12, 13, 1000]
    // The outlier (1000) will be flagged.
    let base = "2025-06-01T12:00:00Z";
    let readings = [(0, 10.0), (1, 11.0), (2, 12.0), (3, 13.0), (4, 1000.0)];
    for (i, val) in &readings {
        common::db::exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
                 VALUES ('{stream}', '{site}', '{param}', '{base}'::timestamptz + interval '{i} minutes', {val}, 0)",
                stream = common::STREAM1_ID,
                site = common::SITE1_ID,
                param = common::GLOBAL_PARAM_TEMP_ID,
            ),
        )
        .await;
    }

    // Refresh aggregates
    common::db::exec(
        &db,
        "CALL refresh_continuous_aggregate('readings_hourly', '2025-06-01', '2025-06-02')",
    )
    .await;

    // Verify baseline: avg includes outlier ≈ 209.2
    let row = db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT avg_value FROM readings_hourly WHERE site_id = '{sid}' AND parameter_id = '{gid}'",
                sid = common::SITE1_ID,
                gid = common::GLOBAL_PARAM_TEMP_ID
            ),
        ))
        .await
        .unwrap()
        .expect("Should have hourly aggregate");
    let avg_before: f64 = row.try_get("", "avg_value").unwrap();
    assert!(avg_before > 200.0, "Baseline avg should include outlier: {avg_before}");

    // Flag the outlier reading
    common::db::exec(
        &db,
        &format!(
            "UPDATE readings SET is_flagged = true, flag_reason = 'outlier' \
             WHERE site_id = '{sid}' AND parameter_id = '{gid}' AND raw_value = 1000.0",
            sid = common::SITE1_ID,
            gid = common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;

    // Refresh aggregates again (this is what the flag handler does)
    common::db::exec(
        &db,
        "CALL refresh_continuous_aggregate('readings_hourly', '2025-06-01', '2025-06-02')",
    )
    .await;

    // Query aggregate after flagging
    let row = db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT avg_value, count FROM readings_hourly WHERE site_id = '{sid}' AND parameter_id = '{gid}'",
                sid = common::SITE1_ID,
                gid = common::GLOBAL_PARAM_TEMP_ID
            ),
        ))
        .await
        .unwrap()
        .expect("Should have hourly aggregate");
    let avg_after: f64 = row.try_get("", "avg_value").unwrap();
    let count: i64 = row.try_get("", "count").unwrap();

    // BUG: This assertion should pass but will FAIL because the aggregate
    // view does not filter is_flagged. avg_after will still be ~209.
    assert!(
        avg_after < 20.0,
        "After flagging outlier, avg should be ~11.5 but got {avg_after} (count={count}). \
         BUG: Continuous aggregate does not exclude flagged readings."
    );
}

// ============================================================================
// Bug 3: Merge silently drops readings on timestamp collision
//
// merge_site_parameters uses ON CONFLICT DO NOTHING.
// Source readings at overlapping timestamps are silently lost.
// ============================================================================

#[tokio::test]
#[serial]
async fn bug3_merge_drops_conflicting_readings() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;

    // Seed project, site, 2 parameters, 2 site_parameters
    common::db::exec(
        &db,
        &format!(
            "INSERT INTO projects (id, name, data_source) VALUES ('{pid}', 'Merge Test', 'test')",
            pid = common::PROJECT_ID
        ),
    )
    .await;
    common::db::exec(
        &db,
        &format!(
            "INSERT INTO sites (id, project_id, name) VALUES ('{sid}', '{pid}', 'Merge Site')",
            sid = common::SITE1_ID,
            pid = common::PROJECT_ID
        ),
    )
    .await;
    common::db::exec(
        &db,
        &format!(
            "INSERT INTO parameters (id, name, display_name, default_units, category, data_type) VALUES \
             ('{p1}', 'Temp_A', 'Temp A', '°C', 'measurement', 'numeric'), \
             ('{p2}', 'Temp_B', 'Temp B', '°C', 'measurement', 'numeric')",
            p1 = common::GLOBAL_PARAM_TEMP_ID,
            p2 = common::GLOBAL_PARAM_DO_ID
        ),
    )
    .await;
    common::db::exec(
        &db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, display_units, sample_interval_sec, is_active) VALUES \
             ('{sp1}', '{sid}', '{p1}', 'Temp_A', '°C', 600, true), \
             ('{sp2}', '{sid}', '{p2}', 'Temp_B', '°C', 600, true)",
            sp1 = common::PARAM_S1_TEMP_ID,
            sp2 = common::PARAM_S1_DO_ID,
            sid = common::SITE1_ID,
            p1 = common::GLOBAL_PARAM_TEMP_ID,
            p2 = common::GLOBAL_PARAM_DO_ID
        ),
    )
    .await;

    // Create data streams for both parameters
    common::seed_data_stream(&db, common::STREAM1_ID, "test", "merge_source").await;
    common::seed_data_stream(&db, common::STREAM2_ID, "test", "merge_target").await;
    // Pair them
    common::db::exec(
        &db,
        &format!(
            "UPDATE data_streams SET site_parameter_id = '{sp}' WHERE id = '{s}'",
            sp = common::PARAM_S1_TEMP_ID,
            s = common::STREAM1_ID,
        ),
    )
    .await;
    common::db::exec(
        &db,
        &format!(
            "UPDATE data_streams SET site_parameter_id = '{sp}' WHERE id = '{s}'",
            sp = common::PARAM_S1_DO_ID,
            s = common::STREAM2_ID,
        ),
    )
    .await;

    // Insert conflicting readings at same timestamp
    // Source (Temp_A): value = 100 at 12:00
    common::db::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{s}', '{sid}', '{p1}', '2025-06-01T12:00:00Z', 100.0, 0)",
            s = common::STREAM1_ID,
            sid = common::SITE1_ID,
            p1 = common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    // Target (Temp_B): value = 200 at 12:00
    common::db::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{s}', '{sid}', '{p2}', '2025-06-01T12:00:00Z', 200.0, 0)",
            s = common::STREAM2_ID,
            sid = common::SITE1_ID,
            p2 = common::GLOBAL_PARAM_DO_ID
        ),
    )
    .await;

    // Also add a non-conflicting reading in source
    common::db::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{s}', '{sid}', '{p1}', '2025-06-01T12:10:00Z', 101.0, 0)",
            s = common::STREAM1_ID,
            sid = common::SITE1_ID,
            p1 = common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;

    // Count readings before merge
    let before_count = db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COUNT(*) as cnt FROM readings".to_string(),
        ))
        .await
        .unwrap()
        .unwrap();
    let count_before: i64 = before_count.try_get("", "cnt").unwrap();
    assert_eq!(count_before, 3, "Should have 3 readings before merge");

    // Perform merge: source → target
    let merge_result = river_db::services::merge::merge_site_parameters(
        &db,
        &river_db::services::merge::MergeSiteParametersRequest {
            source_site_parameter_id: common::PARAM_S1_TEMP_ID.parse().unwrap(),
            target_site_parameter_id: common::PARAM_S1_DO_ID.parse().unwrap(),
        },
    )
    .await;

    assert!(merge_result.is_ok(), "Merge should succeed: {:?}", merge_result.err());
    let result = merge_result.unwrap();

    // BUG: merged_readings reports success but doesn't reveal that the
    // conflicting reading (value=100) was silently dropped.
    // After merge, we should still have access to BOTH values at 12:00.
    let after_count = db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COUNT(*) as cnt FROM readings WHERE site_id = '{sid}' AND parameter_id = '{p2}'",
                sid = common::SITE1_ID,
                p2 = common::GLOBAL_PARAM_DO_ID
            ),
        ))
        .await
        .unwrap()
        .unwrap();
    let count_after: i64 = after_count.try_get("", "cnt").unwrap();

    // We expect 3 readings in the target: original 200 + moved 100 + moved 101
    // But the 100 conflicts with 200 at the same timestamp, so ON CONFLICT drops it.
    // BUG: This should be 3 but will be 2 (the 100 was silently lost)
    assert_eq!(
        count_after, 3,
        "After merge, target should have all 3 readings but got {count_after}. \
         BUG: Conflicting source reading was silently dropped by ON CONFLICT DO NOTHING. \
         merged_readings reported: {}", result.merged_readings
    );
}

// ============================================================================
// Bug 4: Calibration formula - verify actual behavior
// Code: calibrated = slope * raw + intercept
// ============================================================================

#[tokio::test]
async fn bug4_calibration_formula_correctness() {
    // Test the actual formula against expected lab calibration results.
    // The code uses: calibrated = slope * raw + intercept
    let result = river_db::services::calibration::apply_calibration(10.0, 2.0, 5.0);
    assert_eq!(result, 25.0, "calibrated = 2.0 * 10.0 + 5.0 = 25.0");

    // Identity calibration (slope=1, intercept=0)
    let result = river_db::services::calibration::apply_calibration(42.0, 1.0, 0.0);
    assert_eq!(result, 42.0, "Identity calibration should return raw value");

    // Negative intercept
    let result = river_db::services::calibration::apply_calibration(100.0, 1.0, -273.15);
    assert!(
        (result - (-173.15)).abs() < 0.001,
        "Kelvin to Celsius: 100 - 273.15 = -173.15, got {result}"
    );
}

// ============================================================================
// Bug 5: Slope=0 produces constant calibrated_value with no warning
// ============================================================================

#[tokio::test]
async fn bug5_slope_zero_produces_constant_value() {
    let result1 = river_db::services::calibration::apply_calibration(0.0, 0.0, 5.0);
    let result2 = river_db::services::calibration::apply_calibration(100.0, 0.0, 5.0);
    let result3 = river_db::services::calibration::apply_calibration(999.0, 0.0, 5.0);

    // All produce the same value regardless of raw input
    assert_eq!(result1, 5.0);
    assert_eq!(result2, 5.0);
    assert_eq!(result3, 5.0);

    // BUG: This should ideally fail or at least warn.
    // For now, document that slope=0 is silently accepted.
    // The real fix is validation on sensor_calibration create/update.
    // This test passes but documents the problematic behavior.
    panic!(
        "slope=0 calibration silently accepted: ALL inputs produce constant value 5.0. \
         This is almost certainly a data entry error but no validation prevents it."
    );
}

// ============================================================================
// Bug 8: Time range validation off-by-one
// num_days() truncates to integer, so 90 days + 23:59:59 = 90 days
// ============================================================================

#[tokio::test]
#[serial]
async fn bug8_time_range_off_by_one() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;

    // Minimal seed: just project + site + token (no readings needed, we're testing validation)
    common::db::exec(
        &db,
        &format!(
            "INSERT INTO projects (id, name, data_source) VALUES ('{pid}', 'Bug8 Project', 'test')",
            pid = common::PROJECT_ID
        ),
    )
    .await;
    common::db::exec(
        &db,
        &format!(
            "INSERT INTO sites (id, project_id, name) VALUES ('{sid}', '{pid}', 'Bug8 Site')",
            sid = common::SITE1_ID,
            pid = common::PROJECT_ID
        ),
    )
    .await;

    let app = common::build_test_app(db.clone());
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;

    // Query with exactly 90 days — should pass (200 or empty result, not 400)
    let (status, _) = common::get_with_token(
        &app,
        &format!(
            "/api/service/sites/{}/readings?start=2025-01-01T00:00:00Z&end=2025-04-01T00:00:00Z",
            common::SITE1_ID
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "Exactly 90 days should pass validation");

    // Query with 90 days + 23:59:59 — should fail but may pass due to num_days() truncation
    let (status, body) = common::get_with_token(
        &app,
        &format!(
            "/api/service/sites/{}/readings?start=2025-01-01T00:00:00Z&end=2025-04-01T23:59:59Z",
            common::SITE1_ID
        ),
        &token,
    )
    .await;

    // BUG: This should return 400 because the range exceeds 90 days,
    // but num_days() on 90d 23:59:59 returns 90 (truncated).
    assert_eq!(
        status, 400,
        "90 days + 23:59:59 should exceed the 90-day limit but got status {status}. \
         Body: {body}. BUG: num_days() truncates to 90."
    );
}

use sea_orm::ConnectionTrait;
