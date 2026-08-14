//! measurement_type resolution at ingestion: per-reading override → stream-level default →
//! owning sensor's data_frequency ('low' → spot) → 'continuous'. Also covers the register_stream
//! classification semantics (declared value wins, omitted value never clears) and batch tagging.
//!
//! Run: cargo test --test readings -- --test-threads=1

use crate::common::e2e;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

async fn stored_measurement_type(
    db: &DatabaseConnection,
    stream_id: &str,
    raw_value: f64,
) -> String {
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT measurement_type FROM readings \
                 WHERE stream_id = '{stream_id}' AND raw_value = {raw_value}"
            ),
        ))
        .await
        .expect("query")
        .expect("row");
    row.try_get::<Option<String>>("", "measurement_type")
        .expect("measurement_type")
        .unwrap_or_default()
}

async fn register_stream(
    app: &axum::Router,
    token: &str,
    key: &str,
    measurement_type: Option<&str>,
) -> String {
    let mut body = serde_json::json!({"source_system": "mtres", "source_key": key});
    if let Some(mt) = measurement_type {
        body["measurement_type"] = serde_json::json!(mt);
    }
    let (status, stream) =
        crate::common::post_json_parse_with_token(app, "/api/streams/register", &body, token).await;
    assert!(
        (200..300).contains(&status),
        "register ({status}): {stream}"
    );
    e2e::id_of(&stream)
}

async fn ingest_one(
    app: &axum::Router,
    token: &str,
    stream: &str,
    value: f64,
    override_mt: Option<&str>,
) {
    // Distinct time per value: readings share a PK on (stream, time, replicate).
    let time = format!("2025-02-01T{:02}:00:00Z", value as u32);
    let mut reading = serde_json::json!({"time": time, "raw_value": value});
    if let Some(mt) = override_mt {
        reading["measurement_type"] = serde_json::json!(mt);
    }
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/ingest",
        &serde_json::json!({"stream_id": stream, "readings": [reading]}),
        token,
    )
    .await;
    assert_eq!(status, 200, "ingest ({status}): {body}");
    assert_eq!(body["inserted"], 1, "inserted: {body}");
}

#[tokio::test]
#[serial]
async fn ingest_resolution_chain_override_stream_sensor_fallback() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    // Fallback: no override, no stream default, no sensor → continuous.
    let plain = register_stream(&app, &token, "plain", None).await;
    ingest_one(&app, &token, &plain, 1.0, None).await;
    assert_eq!(
        stored_measurement_type(&db, &plain, 1.0).await,
        "continuous"
    );

    // Stream default: declared spot at registration → spot.
    let spot_stream = register_stream(&app, &token, "grabs", Some("spot")).await;
    ingest_one(&app, &token, &spot_stream, 2.0, None).await;
    assert_eq!(
        stored_measurement_type(&db, &spot_stream, 2.0).await,
        "spot"
    );

    // Per-reading override beats the stream default.
    ingest_one(&app, &token, &spot_stream, 3.0, Some("continuous")).await;
    assert_eq!(
        stored_measurement_type(&db, &spot_stream, 3.0).await,
        "continuous"
    );

    // Sensor frequency: low-frequency sensor owning an undeclared stream → spot.
    let (status, sensor) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sensors",
        &serde_json::json!({"name": "lab spectrometer", "data_frequency": "low"}),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "sensor create ({status}): {sensor}"
    );
    let sensor_id = e2e::id_of(&sensor);
    assert_eq!(sensor["data_frequency"], "low", "sensor: {sensor}");

    let lab_stream = register_stream(&app, &token, "lab", None).await;
    crate::common::exec(
        &db,
        &format!("UPDATE data_streams SET sensor_id = '{sensor_id}' WHERE id = '{lab_stream}'"),
    )
    .await;
    ingest_one(&app, &token, &lab_stream, 4.0, None).await;
    assert_eq!(stored_measurement_type(&db, &lab_stream, 4.0).await, "spot");

    // A declared stream default beats the sensor's frequency.
    let mixed = register_stream(&app, &token, "mixed", Some("continuous")).await;
    crate::common::exec(
        &db,
        &format!("UPDATE data_streams SET sensor_id = '{sensor_id}' WHERE id = '{mixed}'"),
    )
    .await;
    ingest_one(&app, &token, &mixed, 5.0, None).await;
    assert_eq!(
        stored_measurement_type(&db, &mixed, 5.0).await,
        "continuous"
    );
}

#[tokio::test]
#[serial]
async fn ingest_skips_invalid_measurement_type_but_registration_refuses_it() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let stream = register_stream(&app, &token, "bad", None).await;

    // A reading is skipped rather than refused: the sync service replaying it cannot correct the
    // payload, and refusing would stall its cursor on this batch forever.
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &serde_json::json!({"stream_id": stream, "readings": [
            {"time": "2025-02-01T00:00:00Z", "raw_value": 1.0, "measurement_type": "grab"}
        ]}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "invalid measurement_type ({status}): {body}");
    assert_eq!(body["inserted"], 0, "nothing lands: {body}");
    assert_eq!(body["skipped"], 1, "the reading is counted: {body}");

    // Registration is a declaration, not a replay: it is refused, so a bad classification cannot
    // become the stream's default and silently misclassify everything that follows.

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &serde_json::json!({"source_system": "mtres", "source_key": "badreg", "measurement_type": "grab"}),
        &token,
    )
    .await;
    assert_eq!(
        status, 400,
        "invalid stream classification ({status}): {body}"
    );
}

#[tokio::test]
#[serial]
async fn register_stream_omitted_classification_never_clears() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let stream = register_stream(&app, &token, "sticky", Some("spot")).await;

    // Re-registration without a classification (a backend that predates the field) keeps 'spot'.
    register_stream(&app, &token, "sticky", None).await;
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT measurement_type FROM data_streams WHERE id = '{stream}'"),
        ))
        .await
        .expect("query")
        .expect("row");
    assert_eq!(
        row.try_get::<Option<String>>("", "measurement_type")
            .unwrap(),
        Some("spot".to_string())
    );

    // A declared classification on re-registration wins.
    register_stream(&app, &token, "sticky", Some("continuous")).await;
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT measurement_type FROM data_streams WHERE id = '{stream}'"),
        ))
        .await
        .expect("query")
        .expect("row");
    assert_eq!(
        row.try_get::<Option<String>>("", "measurement_type")
            .unwrap(),
        Some("continuous".to_string())
    );
}

#[tokio::test]
#[serial]
async fn batch_readings_explicit_spot_is_stored() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let site_id = crate::common::fixtures::SITE1_ID;
    let parameter_id = crate::common::fixtures::GLOBAL_PARAM_DEPTH_ID;

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/batch",
        &serde_json::json!({"readings": [
            {"site_id": site_id, "parameter_id": parameter_id,
             "time": "2025-02-01T00:00:00Z", "raw_value": 7.5, "measurement_type": "spot"},
            {"site_id": site_id, "parameter_id": parameter_id,
             "time": "2025-02-01T01:00:00Z", "raw_value": 8.5}
        ]}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "batch ({status}): {body}");

    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT measurement_type FROM readings WHERE raw_value = 7.5 \
                 AND site_id = '{site_id}' AND parameter_id = '{parameter_id}'"
            ),
        ))
        .await
        .expect("query")
        .expect("row");
    assert_eq!(
        row.try_get::<Option<String>>("", "measurement_type")
            .unwrap(),
        Some("spot".to_string())
    );
}
