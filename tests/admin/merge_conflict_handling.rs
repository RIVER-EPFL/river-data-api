//! Site-parameter merge must not silently drop source readings that collide with the target
//! on (timestamp, replicate).

use sea_orm::ConnectionTrait;
use serial_test::serial;

#[tokio::test]
#[serial]
async fn merge_preserves_conflicting_readings() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    // Seed project, site, 2 parameters, 2 site_parameters
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO projects (id, name, data_source) VALUES ('{pid}', 'Merge Test', 'test')",
            pid = crate::common::PROJECT_ID
        ),
    )
    .await;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO sites (id, project_id, name) VALUES ('{sid}', '{pid}', 'Merge Site')",
            sid = crate::common::SITE1_ID,
            pid = crate::common::PROJECT_ID
        ),
    )
    .await;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category) VALUES \
             ('{p1}', 'Temp_A', 'Temp A', '°C', 'measurement'), \
             ('{p2}', 'Temp_B', 'Temp B', '°C', 'measurement')",
            p1 = crate::common::GLOBAL_PARAM_TEMP_ID,
            p2 = crate::common::GLOBAL_PARAM_DO_ID
        ),
    )
    .await;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, display_units, sample_interval_sec, is_active) VALUES \
             ('{sp1}', '{sid}', '{p1}', 'Temp_A', '°C', 600, true), \
             ('{sp2}', '{sid}', '{p2}', 'Temp_B', '°C', 600, true)",
            sp1 = crate::common::PARAM_S1_TEMP_ID,
            sp2 = crate::common::PARAM_S1_DO_ID,
            sid = crate::common::SITE1_ID,
            p1 = crate::common::GLOBAL_PARAM_TEMP_ID,
            p2 = crate::common::GLOBAL_PARAM_DO_ID
        ),
    )
    .await;

    // Create data streams for both parameters
    crate::common::seed_data_stream(&db, crate::common::STREAM1_ID, "test", "merge_source").await;
    crate::common::seed_data_stream(&db, crate::common::STREAM2_ID, "test", "merge_target").await;
    // Pair them
    crate::common::db::exec(
        &db,
        &format!(
            "UPDATE data_streams SET site_parameter_id = '{sp}' WHERE id = '{s}'",
            sp = crate::common::PARAM_S1_TEMP_ID,
            s = crate::common::STREAM1_ID,
        ),
    )
    .await;
    crate::common::db::exec(
        &db,
        &format!(
            "UPDATE data_streams SET site_parameter_id = '{sp}' WHERE id = '{s}'",
            sp = crate::common::PARAM_S1_DO_ID,
            s = crate::common::STREAM2_ID,
        ),
    )
    .await;

    // Insert conflicting readings at same timestamp
    // Source (Temp_A): value = 100 at 12:00
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{s}', '{sid}', '{p1}', '2025-06-01T12:00:00Z', 100.0, 0)",
            s = crate::common::STREAM1_ID,
            sid = crate::common::SITE1_ID,
            p1 = crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    // Target (Temp_B): value = 200 at 12:00
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{s}', '{sid}', '{p2}', '2025-06-01T12:00:00Z', 200.0, 0)",
            s = crate::common::STREAM2_ID,
            sid = crate::common::SITE1_ID,
            p2 = crate::common::GLOBAL_PARAM_DO_ID
        ),
    )
    .await;

    // Also add a non-conflicting reading in source
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{s}', '{sid}', '{p1}', '2025-06-01T12:10:00Z', 101.0, 0)",
            s = crate::common::STREAM1_ID,
            sid = crate::common::SITE1_ID,
            p1 = crate::common::GLOBAL_PARAM_TEMP_ID
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
    let merge_result = river_db::routes::private::admin::merge_services::merge_site_parameters(
        &db,
        &river_db::routes::private::admin::merge_services::MergeSiteParametersRequest {
            source_site_parameter_id: crate::common::PARAM_S1_TEMP_ID.parse().unwrap(),
            target_site_parameter_id: crate::common::PARAM_S1_DO_ID.parse().unwrap(),
        },
    )
    .await;

    assert!(
        merge_result.is_ok(),
        "Merge should succeed: {:?}",
        merge_result.err()
    );
    let result = merge_result.unwrap();

    // BUG: merged_readings reports success but doesn't reveal that the
    // conflicting reading (value=100) was silently dropped.
    // After merge, we should still have access to BOTH values at 12:00.
    let after_count = db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COUNT(*) as cnt FROM readings WHERE site_id = '{sid}' AND parameter_id = '{p2}'",
                sid = crate::common::SITE1_ID,
                p2 = crate::common::GLOBAL_PARAM_DO_ID
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
         merged_readings reported: {}",
        result.merged_readings
    );
}
