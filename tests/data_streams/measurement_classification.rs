//! Classification of portal history: pairing adopts the stream's declared measurement_type, and
//! `POST /streams/retag` with 'declared' aligns existing readings per stream, which is what a
//! mixed source system (grab columns and logger columns under one system) needs.
//!
//! Run with: cargo test --test data_streams

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

async fn measurement_types(db: &DatabaseConnection, stream_id: Uuid) -> Vec<String> {
    db.query_all(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT COALESCE(measurement_type, 'null') AS mt FROM readings \
             WHERE stream_id = '{stream_id}' ORDER BY time"
        ),
    ))
    .await
    .unwrap()
    .iter()
    .map(|r| r.try_get::<String>("", "mt").unwrap())
    .collect()
}

async fn seed_stream(db: &DatabaseConnection, declared: Option<&str>, tagged_as: Option<&str>) -> Uuid {
    let stream_id = Uuid::new_v4();
    let declared_sql = declared.map_or("NULL".to_string(), |d| format!("'{d}'"));
    let tagged_sql = tagged_as.map_or("NULL".to_string(), |t| format!("'{t}'"));
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, measurement_type, is_active) \
             VALUES ('{stream_id}', 'cnet', '{}', 'CNET column', {declared_sql}, true)",
            Uuid::new_v4()
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, time, replicate_index, raw_value, measurement_type) \
             VALUES ('{stream_id}', '2025-03-01T09:00:00Z', 0, 4.0, {tagged_sql})"
        ),
    )
    .await;
    stream_id
}

#[tokio::test]
#[serial]
async fn pairing_applies_declaration_without_clobbering_reading_tags() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    // A per-reading tag set at ingest outranks the stream declaration; the
    // declaration classifies only untagged (legacy) rows on pairing.
    let tagged = seed_stream(&db, Some("spot"), Some("continuous")).await;
    let untagged = seed_stream(&db, Some("spot"), None).await;

    for (stream_id, site_param) in [
        (tagged, crate::common::PARAM_S1_TEMP_ID),
        (untagged, crate::common::PARAM_S1_DO_ID),
    ] {
        let (status, body) = crate::common::post_json_with_token(
            &app,
            &format!("/api/streams/{stream_id}/pair"),
            &serde_json::json!({ "site_parameter_id": site_param }),
            &token,
        )
        .await;
        assert_eq!(status, 200, "pair ({status}): {body}");
    }

    assert_eq!(
        measurement_types(&db, tagged).await,
        vec!["continuous".to_string()],
        "a per-reading tag survives pairing"
    );
    assert_eq!(
        measurement_types(&db, untagged).await,
        vec!["spot".to_string()],
        "untagged history adopts the stream's declared classification"
    );
}

#[tokio::test]
#[serial]
async fn declared_retag_aligns_each_stream_separately() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let grab = seed_stream(&db, Some("spot"), Some("continuous")).await;
    let logger = seed_stream(&db, Some("continuous"), Some("continuous")).await;
    let undeclared = seed_stream(&db, None, Some("continuous")).await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/streams/retag",
        &serde_json::json!({ "source_system": "cnet", "measurement_type": "declared", "retag_existing": true }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "retag ({status}): {body}");
    let resp: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(resp["streams_updated"], 0, "declared leaves the streams as they are");
    let job_id = resp["job_id"].as_str().expect("a tracked job was enqueued");

    let status = crate::common::e2e::poll_job(&app, &token, job_id, 20).await;
    assert_eq!(status, "completed", "retag job must complete");

    assert_eq!(measurement_types(&db, grab).await, vec!["spot".to_string()]);
    assert_eq!(measurement_types(&db, logger).await, vec!["continuous".to_string()]);
    assert_eq!(
        measurement_types(&db, undeclared).await,
        vec!["continuous".to_string()],
        "a stream with no declaration is left alone"
    );
}
