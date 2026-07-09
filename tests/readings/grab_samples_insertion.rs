
use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;
use uuid::Uuid;

async fn setup() -> (axum::Router, String, sea_orm::DatabaseConnection) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (app, token, db)
}

// ============================================================================
// Basic grab sample insertion — triplicate DOC readings (~192 ppb mean)
// ============================================================================

#[tokio::test]
#[serial]
async fn test_insert_triplicate_grab_samples() {
    let (app, token, db) = setup().await;

    let time = "2025-06-15T10:00:00Z";
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "created_by": "test-user",
            "readings": [
                { "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID, "value": 185.2, "time": time },
                { "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID, "value": 198.7, "time": time },
                { "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID, "value": 191.4, "time": time }
            ]
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "insert should succeed: {body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["inserted"], 3);
    assert_eq!(json["samples_created"], 1, "3 replicates should create 1 sample");

    // Verify readings were inserted with correct site_id and parameter_id
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COUNT(*) as c FROM readings \
                 WHERE site_id = '{}' AND parameter_id = '{}' \
                 AND time = '{time}'",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TEMP_ID
            ),
        ))
        .await
        .unwrap()
        .unwrap();
    let count: i64 = row.try_get("", "c").unwrap();
    assert_eq!(count, 3, "should have 3 readings for this time+parameter");

    // Verify sample row was created and that the trigger populated aggregates
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT n, mean, stdev, min_value, max_value FROM samples \
                 WHERE site_id = '{}' AND parameter_id = '{}'",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TEMP_ID
            ),
        ))
        .await
        .unwrap()
        .unwrap();
    let n: i32 = row.try_get("", "n").unwrap();
    let mean: f64 = row.try_get::<Option<f64>>("", "mean").unwrap().expect("trigger should populate mean");
    let stdev: f64 = row.try_get::<Option<f64>>("", "stdev").unwrap().expect("trigger should populate stdev");
    let min_value: f64 = row.try_get::<Option<f64>>("", "min_value").unwrap().expect("trigger should populate min_value");
    let max_value: f64 = row.try_get::<Option<f64>>("", "max_value").unwrap().expect("trigger should populate max_value");

    // Expected values from the three replicates (185.2, 198.7, 191.4)
    let values = [185.2_f64, 198.7, 191.4];
    let expected_mean = values.iter().sum::<f64>() / 3.0;
    let m = expected_mean;
    let expected_stdev = (values.iter().map(|v| (v - m).powi(2)).sum::<f64>() / 2.0).sqrt();
    assert_eq!(n, 3, "n should be 3");
    assert!((mean - expected_mean).abs() < 1e-9, "mean {mean} should match expected {expected_mean}");
    assert!((stdev - expected_stdev).abs() < 1e-9, "stdev {stdev} should match expected {expected_stdev}");
    assert!((min_value - 185.2).abs() < 1e-9, "min_value should be 185.2");
    assert!((max_value - 198.7).abs() < 1e-9, "max_value should be 198.7");
}

// ============================================================================
// Single reading (no replicates) — should NOT create a sample
// ============================================================================

#[tokio::test]
#[serial]
async fn test_single_grab_sample_no_sample_row() {
    let (app, token, db) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "readings": [
                { "parameter_id": crate::common::GLOBAL_PARAM_DO_ID, "value": 330.0, "time": "2025-06-15T11:00:00Z" }
            ]
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "insert should succeed: {body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["inserted"], 1);
    assert_eq!(json["samples_created"], 0, "single reading should not create a sample");

    // Verify no sample row for this parameter+time
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COUNT(*) as c FROM samples \
                 WHERE site_id = '{}' AND parameter_id = '{}' \
                 AND collected_at = '2025-06-15T11:00:00Z'",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_DO_ID
            ),
        ))
        .await
        .unwrap()
        .unwrap();
    let count: i64 = row.try_get("", "c").unwrap();
    assert_eq!(count, 0);
}

// ============================================================================
// Multiple parameters in one request — mixed replicates
// ============================================================================

#[tokio::test]
#[serial]
async fn test_multi_parameter_grab_samples() {
    let (app, token, _db) = setup().await;

    let time = "2025-06-15T12:00:00Z";
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "readings": [
                { "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID, "value": 7.5, "time": time },
                { "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID, "value": 7.8, "time": time },
                { "parameter_id": crate::common::GLOBAL_PARAM_COND_ID, "value": 432.0, "time": time }
            ]
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "insert should succeed: {body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["inserted"], 3);
    assert_eq!(json["samples_created"], 1, "only Temp has 2+ replicates");
}

// ============================================================================
// Auto-creates grab_sample stream
// ============================================================================

#[tokio::test]
#[serial]
async fn test_grab_sample_creates_stream() {
    let (app, token, db) = setup().await;

    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "readings": [
                { "parameter_id": crate::common::GLOBAL_PARAM_TURB_ID, "value": 25.0, "time": "2025-06-15T13:00:00Z" }
            ]
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200);

    // Verify a grab_sample stream was created
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COUNT(*) as c FROM data_streams \
                 WHERE source_system = 'grab_sample' \
                 AND source_key = '{}:{}'",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TURB_ID
            ),
        ))
        .await
        .unwrap()
        .unwrap();
    let count: i64 = row.try_get("", "c").unwrap();
    assert_eq!(count, 1, "should have created a grab_sample stream");
}

// ============================================================================
// Invalid site returns 404
// ============================================================================

#[tokio::test]
#[serial]
async fn test_grab_sample_invalid_site() {
    let (app, token, _db) = setup().await;

    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": "00000000-0000-4000-a000-999999999999",
            "readings": [
                { "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID, "value": 10.0, "time": "2025-06-15T14:00:00Z" }
            ]
        }),
        &token,
    )
    .await;
    assert_eq!(status, 404);
}

// ============================================================================
// Invalid parameter for site returns 400
// ============================================================================

#[tokio::test]
#[serial]
async fn test_grab_sample_invalid_parameter() {
    let (app, token, _db) = setup().await;

    // Site2 doesn't have DEPTH parameter
    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": crate::common::SITE2_ID,
            "readings": [
                { "parameter_id": crate::common::GLOBAL_PARAM_DEPTH_ID, "value": 300.0, "time": "2025-06-15T15:00:00Z" }
            ]
        }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "DEPTH is not configured for Site2");
}

// ============================================================================
// Empty readings returns 400
// ============================================================================

#[tokio::test]
#[serial]
async fn test_grab_sample_empty_readings() {
    let (app, token, _db) = setup().await;

    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "readings": []
        }),
        &token,
    )
    .await;
    assert_eq!(status, 400);
}

// ============================================================================
// Permission check — write_data required
// ============================================================================

#[tokio::test]
#[serial]
async fn test_grab_samples_require_write_data() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(
        &db,
        serde_json::json!({ "read_data": true, "read_metadata": true }),
        None,
    )
    .await;
    let app = crate::common::build_test_app(db);

    let (status, _) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "readings": [
                { "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID, "value": 10.0, "time": "2025-06-15T16:00:00Z" }
            ]
        }),
        &token,
    )
    .await;
    assert_eq!(status, 403, "grab_samples should require write_data permission");
}

// ============================================================================
// Instant standard curve applied server-side: raw kept, corrected + provenance stored
// ============================================================================

#[tokio::test]
#[serial]
async fn test_grab_applies_instant_curve_server_side() {
    let (app, token, db) = setup().await;

    let sensor_id = "00000000-0000-4000-c000-0000000000a1";
    let curve_id = "00000000-0000-4000-c000-0000000000b1";
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "INSERT INTO sensors (id, name, parameter_id, is_active, is_lab_instrument, created_at)
             VALUES ('{sensor_id}', 'Microplate reader', '{}', true, true, now())",
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    ))
    .await
    .unwrap();
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "INSERT INTO sensor_calibrations (id, sensor_id, slope, intercept, valid_from, mode, name)
             VALUES ('{curve_id}', '{sensor_id}', 2.0, 1.0, now(), 'instant', 'Plate A')"
        ),
    ))
    .await
    .unwrap();

    let time = "2025-07-01T09:00:00Z";
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "readings": [
                { "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID, "sensor_id": sensor_id,
                  "calibration_id": curve_id, "value": 10.0, "time": time }
            ]
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab with curve should succeed: {body}");

    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT raw_value, calibrated_value, calibration_id, measurement_type FROM readings \
                 WHERE site_id = '{}' AND parameter_id = '{}' AND time = '{time}'",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TEMP_ID
            ),
        ))
        .await
        .unwrap()
        .unwrap();
    let raw: f64 = row.try_get("", "raw_value").unwrap();
    let calibrated: f64 = row.try_get("", "calibrated_value").unwrap();
    let stored_curve: Uuid = row.try_get("", "calibration_id").unwrap();
    let mtype: String = row.try_get("", "measurement_type").unwrap();
    assert_eq!(raw, 10.0, "raw value is the measured value");
    assert_eq!(calibrated, 21.0, "2.0 * 10.0 + 1.0");
    assert_eq!(stored_curve.to_string(), curve_id, "applied curve stamped for provenance");
    assert_eq!(mtype, "spot");
}

#[tokio::test]
#[serial]
async fn test_grab_rejects_unknown_curve() {
    let (app, token, _db) = setup().await;
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "readings": [
                { "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
                  "calibration_id": "00000000-0000-4000-c000-0000000000ff",
                  "value": 10.0, "time": "2025-07-01T10:00:00Z" }
            ]
        }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "unknown curve id should be a 400: {body}");
}
