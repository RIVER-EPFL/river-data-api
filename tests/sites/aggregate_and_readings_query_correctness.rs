//! Aggregate + readings correctness: flagged readings are excluded from continuous aggregates,
//! and the readings endpoint accepts inclusive time-range boundaries.

use sea_orm::ConnectionTrait;
use serial_test::serial;

#[tokio::test]
#[serial]
async fn flagged_readings_excluded_from_aggregates() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    // Seed minimal data: 1 project, 1 site, 1 parameter, 1 site_parameter
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO projects (id, name, data_source) VALUES ('{pid}', 'Bug1 Project', 'test')",
            pid = crate::common::PROJECT_ID
        ),
    )
    .await;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO sites (id, project_id, name) VALUES ('{sid}', '{pid}', 'Bug1 Site')",
            sid = crate::common::SITE1_ID,
            pid = crate::common::PROJECT_ID
        ),
    )
    .await;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
             VALUES ('{gid}', 'Temperature', 'Temperature', '°C', 'measurement')",
            gid = crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, display_units, sample_interval_sec, is_active) \
             VALUES ('{spid}', '{sid}', '{gid}', 'Temperature', '°C', 600, true)",
            spid = crate::common::PARAM_S1_TEMP_ID,
            sid = crate::common::SITE1_ID,
            gid = crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;

    // Create a data stream (readings require stream_id)
    crate::common::seed_data_stream(&db, crate::common::STREAM1_ID, "test", "bug1_stream").await;
    // Pair it to the site_parameter
    crate::common::db::exec(
        &db,
        &format!(
            "UPDATE data_streams SET site_parameter_id = '{spid}' WHERE id = '{sid}'",
            spid = crate::common::PARAM_S1_TEMP_ID,
            sid = crate::common::STREAM1_ID
        ),
    )
    .await;

    // Insert 5 readings in the same hour bucket: [10, 11, 12, 13, 1000]
    // The outlier (1000) will be flagged.
    let base = "2025-06-01T12:00:00Z";
    let readings = [(0, 10.0), (1, 11.0), (2, 12.0), (3, 13.0), (4, 1000.0)];
    for (i, val) in &readings {
        crate::common::db::exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
                 VALUES ('{stream}', '{site}', '{param}', '{base}'::timestamptz + interval '{i} minutes', {val}, 0)",
                stream = crate::common::STREAM1_ID,
                site = crate::common::SITE1_ID,
                param = crate::common::GLOBAL_PARAM_TEMP_ID,
            ),
        )
        .await;
    }

    // Refresh aggregates
    crate::common::db::exec(
        &db,
        "CALL refresh_continuous_aggregate('readings_hourly', '2025-06-01', '2025-06-02')",
    )
    .await;

    // Verify baseline: avg includes outlier ≈ 209.2
    let row = db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT avg_value FROM readings_hourly \
                 WHERE site_id = '{sid}' AND parameter_id = '{gid}' \
                 AND bucket >= '2025-06-01' AND bucket < '2025-06-02'",
                sid = crate::common::SITE1_ID,
                gid = crate::common::GLOBAL_PARAM_TEMP_ID
            ),
        ))
        .await
        .unwrap()
        .expect("Should have hourly aggregate");
    let avg_before: f64 = row.try_get("", "avg_value").unwrap();
    assert!(
        avg_before > 200.0,
        "Baseline avg should include outlier: {avg_before}"
    );

    // Flag the outlier reading
    crate::common::db::exec(
        &db,
        &format!(
            "UPDATE readings SET is_flagged = true, flag_reason = 'outlier' \
             WHERE site_id = '{sid}' AND parameter_id = '{gid}' AND raw_value = 1000.0",
            sid = crate::common::SITE1_ID,
            gid = crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;

    // Refresh aggregates again (this is what the flag handler does)
    crate::common::db::exec(
        &db,
        "CALL refresh_continuous_aggregate('readings_hourly', '2025-06-01', '2025-06-02')",
    )
    .await;

    // Query aggregate after flagging
    let row = db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT avg_value, count FROM readings_hourly \
                 WHERE site_id = '{sid}' AND parameter_id = '{gid}' \
                 AND bucket >= '2025-06-01' AND bucket < '2025-06-02'",
                sid = crate::common::SITE1_ID,
                gid = crate::common::GLOBAL_PARAM_TEMP_ID
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

#[tokio::test]
#[serial]
async fn readings_time_range_boundaries_accepted() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO projects (id, name, data_source) VALUES ('{pid}', 'Bug8 Project', 'test')",
            pid = crate::common::PROJECT_ID
        ),
    )
    .await;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO sites (id, project_id, name) VALUES ('{sid}', '{pid}', 'Bug8 Site')",
            sid = crate::common::SITE1_ID,
            pid = crate::common::PROJECT_ID
        ),
    )
    .await;

    let app = crate::common::build_test_app(db.clone());
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;

    let (status, _) = crate::common::get_with_token(
        &app,
        &format!(
            "/api/sites/{}/readings?start=2025-01-01T00:00:00Z&end=2025-04-01T00:00:00Z",
            crate::common::SITE1_ID
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200);

    let (status, _) = crate::common::get_with_token(
        &app,
        &format!(
            "/api/sites/{}/readings?start=2025-01-01T00:00:00Z&end=2025-04-01T23:59:59Z",
            crate::common::SITE1_ID
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200);
}

/// `measurement_type=continuous` covers both explicit continuous rows and legacy rows written
/// before the column existed (NULL), while `spot` returns only grab samples. This is the query
/// contract the High/Low frequency toggle relies on.
#[tokio::test]
#[serial]
async fn measurement_type_continuous_includes_legacy_null_rows() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO projects (id, name, data_source) VALUES ('{pid}', 'Freq Project', 'test')",
            pid = crate::common::PROJECT_ID
        ),
    )
    .await;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO sites (id, project_id, name) VALUES ('{sid}', '{pid}', 'Freq Site')",
            sid = crate::common::SITE1_ID,
            pid = crate::common::PROJECT_ID
        ),
    )
    .await;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
             VALUES ('{gid}', 'Temperature', 'Temperature', '°C', 'measurement')",
            gid = crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, display_units, sample_interval_sec, is_active) \
             VALUES ('{spid}', '{sid}', '{gid}', 'Temperature', '°C', 600, true)",
            spid = crate::common::PARAM_S1_TEMP_ID,
            sid = crate::common::SITE1_ID,
            gid = crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    crate::common::seed_data_stream(&db, crate::common::STREAM1_ID, "test", "freq_stream").await;
    crate::common::db::exec(
        &db,
        &format!(
            "UPDATE data_streams SET site_parameter_id = '{spid}' WHERE id = '{sid}'",
            spid = crate::common::PARAM_S1_TEMP_ID,
            sid = crate::common::STREAM1_ID
        ),
    )
    .await;

    let base = "2025-07-01T12:00:00Z";
    // 3 legacy rows (measurement_type NULL) + 2 explicit continuous + 2 spot, at distinct minutes.
    let rows: [(i64, &str); 7] = [
        (0, "NULL"),
        (1, "NULL"),
        (2, "NULL"),
        (3, "'continuous'"),
        (4, "'continuous'"),
        (5, "'spot'"),
        (6, "'spot'"),
    ];
    for (i, mt) in &rows {
        crate::common::db::exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index, measurement_type) \
                 VALUES ('{stream}', '{site}', '{param}', '{base}'::timestamptz + interval '{i} minutes', {i}, 0, {mt})",
                stream = crate::common::STREAM1_ID,
                site = crate::common::SITE1_ID,
                param = crate::common::GLOBAL_PARAM_TEMP_ID,
            ),
        )
        .await;
    }

    let app = crate::common::build_test_app(db.clone());
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let range = "start=2025-07-01T00:00:00Z&end=2025-07-02T00:00:00Z";

    let count_times =
        |body: &serde_json::Value| body["times"].as_array().map(|a| a.len()).unwrap_or(0);

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/sites/{}/readings?{range}&measurement_type=continuous",
            crate::common::SITE1_ID
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        count_times(&body),
        5,
        "continuous should include the 3 NULL + 2 continuous rows: {body}"
    );

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/sites/{}/readings?{range}&measurement_type=spot",
            crate::common::SITE1_ID
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        count_times(&body),
        2,
        "spot should return only the 2 grab samples: {body}"
    );

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{}/readings?{range}", crate::common::SITE1_ID),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(count_times(&body), 7, "no filter returns all rows: {body}");
}
