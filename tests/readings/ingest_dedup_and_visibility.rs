//! Ingestion idempotency + visibility gaps the doc promises but the suite didn't assert:
//! duplicate readings/status-events are first-write-wins on the hypertable PK, the batch endpoint's
//! skip-vs-overwrite conflict modes, and unpaired-stream readings stay out of continuous aggregates
//! until the stream is paired.
//!
//! Run: cargo test --test readings -- --test-threads=1


use crate::common::e2e;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    let row = db
        .query_one(Statement::from_string(DatabaseBackend::Postgres, sql.to_string()))
        .await
        .expect("query")
        .expect("row");
    row.try_get::<i64>("", "c").expect("c")
}

async fn register_stream(app: &axum::Router, token: &str, key: &str) -> String {
    let (status, stream) = crate::common::post_json_parse_with_token(
        app,
        "/api/streams/register",
        &serde_json::json!({"source_system": "dedup", "source_key": key}),
        token,
    )
    .await;
    assert!((200..300).contains(&status), "register ({status}): {stream}");
    e2e::id_of(&stream)
}

#[tokio::test]
#[serial]
async fn ingest_duplicate_reading_first_write_wins() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let stream = register_stream(&app, &token, "r1").await;

    let t = "2025-01-15T00:00:00Z";
    let (status, body) = crate::common::post_json_parse_with_token(
        &app, "/api/ingest",
        &serde_json::json!({"stream_id": stream, "readings": [{"time": t, "raw_value": 10.0}]}),
        &token,
    ).await;
    assert_eq!(status, 200, "first ingest ({status}): {body}");
    assert_eq!(body["inserted"], 1);

    let (status, body) = crate::common::post_json_parse_with_token(
        &app, "/api/ingest",
        &serde_json::json!({"stream_id": stream, "readings": [{"time": t, "raw_value": 99.0}]}),
        &token,
    ).await;
    assert_eq!(status, 200, "dup ingest ({status}): {body}");
    assert_eq!(body["inserted"], 0, "duplicate (stream,time,replicate) skipped");

    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM readings WHERE stream_id = '{stream}'")).await,
        1, "only one row stored"
    );
    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM readings WHERE stream_id = '{stream}' AND raw_value = 10")).await,
        1, "first write wins"
    );
    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM readings WHERE stream_id = '{stream}' AND raw_value = 99")).await,
        0, "second write dropped"
    );
}

#[tokio::test]
#[serial]
async fn ingest_status_event_dedup_first_write_wins() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let stream = register_stream(&app, &token, "se1").await;

    let t1 = "2025-01-15T00:00:00Z";
    let t2 = "2025-01-15T01:00:00Z";
    let t3 = "2025-01-15T02:00:00Z";
    let (status, body) = crate::common::post_json_parse_with_token(
        &app, "/api/ingest/status_events",
        &serde_json::json!({"stream_id": stream, "events": [
            {"time": t1, "value": "ok"}, {"time": t2, "value": "ok"}, {"time": t3, "value": "ok"}
        ]}),
        &token,
    ).await;
    assert_eq!(status, 200, "first status ingest ({status}): {body}");

    // re-send two existing timestamps (with different values) + one new
    let t4 = "2025-01-15T03:00:00Z";
    let (status, body) = crate::common::post_json_parse_with_token(
        &app, "/api/ingest/status_events",
        &serde_json::json!({"stream_id": stream, "events": [
            {"time": t1, "value": "changed"}, {"time": t2, "value": "changed"}, {"time": t4, "value": "ok"}
        ]}),
        &token,
    ).await;
    assert_eq!(status, 200, "overlapping status ingest ({status}): {body}");

    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM status_events WHERE stream_id = '{stream}'")).await,
        4, "only the one genuinely-new timestamp was added"
    );
    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM status_events WHERE stream_id = '{stream}' AND value = 'changed'")).await,
        0, "existing (stream,time) keeps its first value"
    );
}

#[tokio::test]
#[serial]
async fn batch_reading_conflict_skip_then_overwrite() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let project = e2e::create_project(&app, &token, "Batch P", "batch-p", false).await;
    let site = e2e::create_site(&app, &token, &project, "Batch Site", "batch-site").await;
    let param = e2e::create_parameter(&app, &token, "batchcond", "Conductivity", "uS/cm").await;
    let t = "2025-01-15T00:00:00Z";

    let body = serde_json::json!({"readings": [
        {"site_id": site, "parameter_id": param, "time": t, "raw_value": 10.0}
    ]});
    let (status, resp) = crate::common::post_json_parse_with_token(&app, "/api/readings/batch", &body, &token).await;
    assert_eq!(status, 200, "first batch ({status}): {resp}");
    assert_eq!(resp["inserted"], 1);

    // default conflict = skip → first write wins
    let body = serde_json::json!({"readings": [
        {"site_id": site, "parameter_id": param, "time": t, "raw_value": 99.0}
    ]});
    let (status, resp) = crate::common::post_json_parse_with_token(&app, "/api/readings/batch", &body, &token).await;
    assert_eq!(status, 200, "skip batch ({status}): {resp}");
    assert_eq!(resp["inserted"], 0, "collision skipped by default");
    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM readings WHERE site_id = '{site}' AND raw_value = 10")).await,
        1, "skip kept the original value"
    );

    // explicit overwrite → value replaced
    let body = serde_json::json!({"readings": [
        {"site_id": site, "parameter_id": param, "time": t, "raw_value": 77.0}
    ], "conflict": "overwrite"});
    let (status, resp) = crate::common::post_json_parse_with_token(&app, "/api/readings/batch", &body, &token).await;
    assert_eq!(status, 200, "overwrite batch ({status}): {resp}");
    assert_eq!(resp["overwritten"], 1, "overwrite replaced the row");
    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM readings WHERE site_id = '{site}' AND raw_value = 77")).await,
        1, "overwrite stored the new value"
    );
}

#[tokio::test]
#[serial]
async fn unpaired_readings_excluded_from_aggregates_until_paired() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let project = e2e::create_project(&app, &token, "Agg P", "agg-p", false).await;
    let site = e2e::create_site(&app, &token, &project, "Agg Site", "agg-site").await;
    let param = e2e::create_parameter(&app, &token, "aggcond", "Conductivity", "uS/cm").await;
    let sp = e2e::assign_site_parameter_minimal(&app, &token, &site, &param).await;
    let stream = register_stream(&app, &token, "agg1").await;

    // Ingest onto the UNPAIRED stream — readings carry site_id = NULL.
    let readings: Vec<serde_json::Value> = (0..6)
        .map(|i| serde_json::json!({"time": format!("2025-01-15T0{i}:00:00Z"), "raw_value": 100.0 + i as f64}))
        .collect();
    let (status, body) = crate::common::post_json_parse_with_token(
        &app, "/api/ingest",
        &serde_json::json!({"stream_id": stream, "readings": readings}),
        &token,
    ).await;
    assert_eq!(status, 200, "ingest ({status}): {body}");
    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM readings WHERE stream_id = '{stream}' AND site_id IS NULL")).await,
        6, "unpaired readings have NULL site_id"
    );

    crate::common::refresh_continuous_aggregates(&db).await;
    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM readings_hourly WHERE site_id = '{site}'")).await,
        0, "unpaired readings are excluded from the continuous aggregate"
    );

    // Pair the stream → backfill stamps site_id onto the existing readings.
    let (status, body) = crate::common::post_json_parse_with_token(
        &app, &format!("/api/streams/{stream}/pair"),
        &serde_json::json!({"site_parameter_id": sp}),
        &token,
    ).await;
    assert_eq!(status, 200, "pair ({status}): {body}");
    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM readings WHERE stream_id = '{stream}' AND site_id = '{site}'")).await,
        6, "pairing backfilled site_id"
    );

    crate::common::refresh_continuous_aggregates(&db).await;
    assert!(
        count(&db, &format!("SELECT count(*) AS c FROM readings_hourly WHERE site_id = '{site}'")).await > 0,
        "after pairing the readings appear in the continuous aggregate"
    );
}

#[tokio::test]
#[serial]
async fn ingest_overwrite_updates_values_and_is_sync_only() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (sync_token, _service_id) = crate::common::seed_sync_session_token(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let stream = register_stream(&app, &token, "ow1").await;

    let t = "2025-01-15T00:00:00Z";
    let (status, body) = crate::common::post_json_parse_with_token(
        &app, "/api/ingest",
        &serde_json::json!({"stream_id": stream, "readings": [{"time": t, "raw_value": 10.0}]}),
        &sync_token,
    ).await;
    assert_eq!(status, 200, "first ingest ({status}): {body}");

    db.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "UPDATE readings SET is_flagged = TRUE, flag_reason = 'manual' WHERE stream_id = '{stream}'"
        ),
    ))
    .await
    .expect("flag reading");

    let (status, body) = crate::common::post_json_parse_with_token(
        &app, "/api/ingest",
        &serde_json::json!({"stream_id": stream, "overwrite": true, "readings": [{"time": t, "raw_value": 42.5}]}),
        &sync_token,
    ).await;
    assert_eq!(status, 200, "overwrite ingest ({status}): {body}");

    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM readings WHERE stream_id = '{stream}' AND raw_value = 42.5")).await,
        1, "correction applied in place"
    );
    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM readings WHERE stream_id = '{stream}'")).await,
        1, "still one row"
    );
    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM readings WHERE stream_id = '{stream}' AND is_flagged AND flag_reason = 'manual'"
        )).await,
        1, "operator flag survives the overwrite"
    );

    let (status, _) = crate::common::post_json_with_token(
        &app, "/api/ingest",
        &serde_json::json!({"stream_id": stream, "overwrite": true, "readings": [{"time": t, "raw_value": 1.0}]}),
        &token,
    ).await;
    assert_eq!(status, 403, "overwrite is refused for API tokens");
}
