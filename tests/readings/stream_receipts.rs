//! The stream receipts ledger endpoint and the withdrawn count on stream stats.
//!
//! Run: cargo test --test readings stream_receipts -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::PARAM_S1_DO_ID;

const T1: &str = "2025-06-01T08:00:00Z";

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

async fn paired_spot_stream(
    db: &DatabaseConnection,
    app: &axum::Router,
    token: &str,
) -> (String, String) {
    let (sync_token, _service) = crate::common::seed_sync_session_token(db).await;
    let (status, stream) = crate::common::post_json_parse_with_token(
        app,
        "/api/streams/register",
        &json!({"source_system": "cnet", "source_key": "stn:doc:reps", "measurement_type": "spot"}),
        token,
    )
    .await;
    assert!((200..300).contains(&status), "{stream}");
    let stream_id = crate::common::e2e::id_of(&stream);
    let (status, body) = crate::common::post_json_with_token(
        app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({"site_parameter_id": PARAM_S1_DO_ID}),
        token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    (stream_id, sync_token)
}

#[tokio::test]
#[serial]
async fn a_windowed_pass_is_readable_from_the_ledger() {
    let (db, app, token) = setup().await;
    let (stream_id, sync_token) = paired_spot_stream(&db, &app, &token).await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/ingest",
        &json!({
            "stream_id": stream_id,
            "collection": true,
            "readings": [
                { "time": T1, "raw_value": 10.0, "replicate_index": 0 },
                { "time": T1, "raw_value": 12.0, "replicate_index": 1 },
            ],
            "window": {
                "from": "2025-06-01T00:00:00Z",
                "to": "2025-06-02T00:00:00Z",
                "source_rows_read": 1,
            },
        }),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/receipts"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 1);
    let receipt = &body["receipts"][0];
    assert_eq!(receipt["submitted"], 2);
    assert_eq!(receipt["new_rows"], 2);
    assert_eq!(receipt["withdrawn"], 0);
    assert_eq!(receipt["braked"], false);
    assert_eq!(receipt["window_from"], "2025-06-01T00:00:00Z");

    // The same pass is the covering receipt on the resolver's origin section.
    let (status, prov) = crate::common::get_json_with_token(
        &app,
        &format!("/api/readings/provenance?stream_id={stream_id}&time={T1}"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{prov}");
    assert_eq!(prov["records"][0]["origin"]["receipt"]["id"], receipt["id"]);
}

#[tokio::test]
#[serial]
async fn stream_stats_count_withdrawn_rows() {
    let (db, app, token) = setup().await;
    let (stream_id, sync_token) = paired_spot_stream(&db, &app, &token).await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/ingest",
        &json!({
            "stream_id": stream_id,
            "readings": [
                { "time": T1, "raw_value": 10.0, "replicate_index": 0 },
                { "time": T1, "raw_value": 12.0, "replicate_index": 1 },
            ],
        }),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    db.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "UPDATE readings SET withdrawn_at = NOW() \
             WHERE stream_id = '{stream_id}' AND replicate_index = 1"
        ),
    ))
    .await
    .unwrap();

    let (status, stats) = crate::common::get_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/stats"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{stats}");
    assert_eq!(stats["reading_count"], 2, "withdrawn rows stay counted");
    assert_eq!(stats["withdrawn_count"], 1);
}
