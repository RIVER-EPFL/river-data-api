
use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;

async fn setup() -> (axum::Router, String, sea_orm::DatabaseConnection) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (app, token, db)
}

// ============================================================================
// Stream registration
// ============================================================================

#[tokio::test]
#[serial]
async fn test_register_stream() {
    let (app, token, _db) = setup().await;

    let (status, json) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &serde_json::json!({
            "source_system": "vaisala",
            "source_key": "9999",
            "source_name": "TestSensor",
            "source_path": "viewLinc/BREATHE/Martigny",
            "metadata": { "location_id": 9999 }
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "register should return 200");
    assert_eq!(json["source_system"], "vaisala");
    assert_eq!(json["source_key"], "9999");
    assert_eq!(json["source_name"], "TestSensor");
    assert!(json["site_parameter_id"].is_null(), "new stream should be unpaired");
    assert!(json["paired_at"].is_null());
}

#[tokio::test]
#[serial]
async fn test_register_stream_upsert() {
    let (app, token, _db) = setup().await;

    // First registration
    let (status, json1) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &serde_json::json!({
            "source_system": "test-upsert",
            "source_key": "key-1",
            "source_name": "Original Name"
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    let id1 = json1["id"].as_str().unwrap().to_string();

    // Second registration with same source_system + source_key — should upsert
    let (status, json2) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &serde_json::json!({
            "source_system": "test-upsert",
            "source_key": "key-1",
            "source_name": "Updated Name"
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json2["id"].as_str().unwrap(), id1, "upsert should return same ID");
    assert_eq!(json2["source_name"], "Updated Name");
}

// ============================================================================
// Stream pairing — pair, verify backfill, check stats
// ============================================================================

#[tokio::test]
#[serial]
async fn test_pair_stream_backfills_readings() {
    let (app, token, db) = setup().await;

    // Create an unpaired stream with pre-existing readings (no site_id)
    let stream_id = "00000000-0000-4000-e000-000000000001";
    crate::common::seed_data_stream(&db, stream_id, "test-pair", "pair-key-1").await;

    let bt = crate::common::base_time();
    for i in 0..10 {
        let time = bt + chrono::Duration::minutes(i * 10);
        let value = 330.0 + (i as f64) * 0.5;
        db.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "INSERT INTO readings (stream_id, time, raw_value) VALUES ('{stream_id}', '{}', {value})",
                time.to_rfc3339()
            ),
        ))
        .await
        .unwrap();
    }

    // Verify readings have no site_id yet
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT COUNT(*) as c FROM readings WHERE stream_id = '{stream_id}' AND site_id IS NULL"),
        ))
        .await
        .unwrap()
        .unwrap();
    let unpaired_count: i64 = row.try_get("", "c").unwrap();
    assert_eq!(unpaired_count, 10, "all readings should be unpaired before pairing");

    // Pair the stream
    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &serde_json::json!({ "site_parameter_id": crate::common::PARAM_S1_TEMP_ID }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "pair should succeed: {body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["backfilled"], 10, "should backfill 10 readings");
    assert!(!json["stream"]["site_parameter_id"].is_null(), "stream should now be paired");

    // Verify readings now have site_id and parameter_id
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COUNT(*) as c FROM readings WHERE stream_id = '{stream_id}' AND site_id = '{}'",
                crate::common::SITE1_ID
            ),
        ))
        .await
        .unwrap()
        .unwrap();
    let paired_count: i64 = row.try_get("", "c").unwrap();
    assert_eq!(paired_count, 10, "all readings should have site_id after pairing");

    // Verify calibrated_value was set
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COUNT(*) as c FROM readings WHERE stream_id = '{stream_id}' AND calibrated_value IS NOT NULL"
            ),
        ))
        .await
        .unwrap()
        .unwrap();
    let cal_count: i64 = row.try_get("", "c").unwrap();
    assert_eq!(cal_count, 10, "all readings should have calibrated_value after pairing");
}

#[tokio::test]
#[serial]
async fn test_pair_already_paired_stream_fails() {
    let (app, token, db) = setup().await;

    let stream_id = "00000000-0000-4000-e000-000000000002";
    crate::common::seed_paired_stream(&db, stream_id, "test-pair", "pair-key-2", crate::common::PARAM_S1_DO_ID).await;

    let (status, _) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &serde_json::json!({ "site_parameter_id": crate::common::PARAM_S1_TEMP_ID }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "pairing an already-paired stream should fail");
}

// ============================================================================
// Stream unpairing — unpair, verify site_id/parameter_id cleared
// ============================================================================

#[tokio::test]
#[serial]
async fn test_unpair_stream_clears_readings() {
    let (app, token, db) = setup().await;

    // Create a paired stream with readings that have site_id set
    let stream_id = "00000000-0000-4000-e000-000000000003";
    crate::common::seed_paired_stream(&db, stream_id, "test-unpair", "unpair-key-1", crate::common::PARAM_S1_COND_ID).await;

    let bt = crate::common::base_time();
    for i in 0..5 {
        let time = bt + chrono::Duration::minutes(i * 10);
        db.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value) \
                 VALUES ('{stream_id}', '{}', '{}', '{}', {})",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_COND_ID,
                time.to_rfc3339(),
                450.0 + i as f64
            ),
        ))
        .await
        .unwrap();
    }

    // Unpair
    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/unpair"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "unpair should succeed: {body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["cleared"], 5, "should clear 5 readings");
    assert!(json["stream"]["site_parameter_id"].is_null(), "stream should be unpaired");

    // Verify readings have site_id = NULL
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT COUNT(*) as c FROM readings WHERE stream_id = '{stream_id}' AND site_id IS NULL"),
        ))
        .await
        .unwrap()
        .unwrap();
    let count: i64 = row.try_get("", "c").unwrap();
    assert_eq!(count, 5, "all readings should have site_id=NULL after unpairing");
}

#[tokio::test]
#[serial]
async fn test_unpair_unpaired_stream_fails() {
    let (app, token, db) = setup().await;

    let stream_id = "00000000-0000-4000-e000-000000000004";
    crate::common::seed_data_stream(&db, stream_id, "test-unpair", "unpair-key-2").await;

    let (status, _) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/unpair"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 400, "unpairing an unpaired stream should fail");
}

// ============================================================================
// Stream stats
// ============================================================================

#[tokio::test]
#[serial]
async fn test_stream_stats() {
    let (app, token, db) = setup().await;

    let stream_id = "00000000-0000-4000-e000-000000000005";
    crate::common::seed_data_stream(&db, stream_id, "test-stats", "stats-key-1").await;

    let bt = crate::common::base_time();
    for i in 0..20 {
        let time = bt + chrono::Duration::minutes(i * 10);
        let value = 290.0 + (i as f64) * 1.5;
        db.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "INSERT INTO readings (stream_id, time, raw_value) VALUES ('{stream_id}', '{}', {value})",
                time.to_rfc3339()
            ),
        ))
        .await
        .unwrap();
    }

    let (status, json) = crate::common::get_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/stats"),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json["stream_id"], stream_id);
    assert_eq!(json["reading_count"], 20);
    assert!(!json["min_time"].is_null());
    assert!(!json["max_time"].is_null());

    let latest = json["latest_value"].as_f64().unwrap();
    assert!(
        (latest - 318.5).abs() < 0.1,
        "latest value should be 290 + 19*1.5 = 318.5, got {latest}"
    );
}

#[tokio::test]
#[serial]
async fn test_stream_stats_empty() {
    let (app, token, db) = setup().await;

    let stream_id = "00000000-0000-4000-e000-000000000006";
    crate::common::seed_data_stream(&db, stream_id, "test-stats", "stats-key-2").await;

    let (status, json) = crate::common::get_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/stats"),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(json["reading_count"], 0);
    assert!(json["min_time"].is_null());
    assert!(json["latest_value"].is_null());
}

#[tokio::test]
#[serial]
async fn test_stream_stats_not_found() {
    let (app, token, _db) = setup().await;

    let (status, _) = crate::common::get_with_token(
        &app,
        "/api/streams/00000000-0000-4000-e000-999999999999/stats",
        &token,
    )
    .await;
    assert_eq!(status, 404);
}

// ============================================================================
// Permission checks
// ============================================================================

#[tokio::test]
#[serial]
async fn test_streams_require_write_metadata() {
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
        "/api/streams/register",
        &serde_json::json!({
            "source_system": "test",
            "source_key": "perm-check"
        }),
        &token,
    )
    .await;
    assert_eq!(status, 403, "stream registration should require write_metadata");
}
