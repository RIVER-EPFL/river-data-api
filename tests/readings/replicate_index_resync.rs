//! `replicate_index` is the source's column position and nothing renumbers it, so a full resync
//! replays the same primary keys and `/ingest`'s conflict-do-nothing absorbs them: the group's
//! membership and its sample statistics are the same after the replay as before it.
//!
//! Run: cargo test --test readings replicate_index_resync -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

const T1: &str = "2025-06-01T08:00:00Z";

struct Fixture {
    db: DatabaseConnection,
    app: axum::Router,
    sync_token: String,
    stream: String,
}

async fn setup(key: &str) -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (sync_token, _service_id) = crate::common::seed_sync_session_token(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({"source_system": "resyncsrc", "source_key": key, "measurement_type": "spot"}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "register: {stream}");
    let stream = crate::common::e2e::id_of(&stream);

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream}/pair"),
        &json!({"site_parameter_id": crate::common::PARAM_S1_TEMP_ID}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "pair ({status}): {body}");

    Fixture {
        db,
        app,
        sync_token,
        stream,
    }
}

fn sparse_group(time: &str, members: &[(i16, f64)]) -> Vec<serde_json::Value> {
    members
        .iter()
        .map(|(i, v)| json!({"time": time, "raw_value": v, "replicate_index": i}))
        .collect()
}

async fn ingest(fx: &Fixture, readings: Vec<serde_json::Value>) -> serde_json::Value {
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/ingest",
        &json!({"stream_id": fx.stream, "readings": readings}),
        &fx.sync_token,
    )
    .await;
    assert_eq!(status, 200, "ingest ({status}): {body}");
    body
}

async fn stored_indexes(fx: &Fixture) -> Vec<i16> {
    fx.db
        .query_all(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT replicate_index FROM readings \
                 WHERE stream_id = '{}' AND time = '{T1}' ORDER BY replicate_index",
                fx.stream
            ),
        ))
        .await
        .unwrap()
        .iter()
        .map(|row| row.try_get::<i16>("", "replicate_index").unwrap())
        .collect()
}

async fn sample_mean_and_n(fx: &Fixture) -> (f64, i64) {
    let row = fx
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT mean, n::bigint AS n FROM samples \
                 WHERE site_id = '{}' AND parameter_id = '{}' AND collected_at = '{T1}'",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TEMP_ID
            ),
        ))
        .await
        .unwrap()
        .expect("the group formed a sample");
    (
        row.try_get::<f64>("", "mean").unwrap(),
        row.try_get::<i64>("", "n").unwrap(),
    )
}

/// Scenario: a portal omits a NULL first column, so the family arrives at indexes 1 and 2, and the
/// source later replays its whole history.
/// Expected behaviour: the replay changes nothing. Renumbering the group to 0 and 1 on first
/// ingest would leave index 2 free, and the replay would insert a third reading there.
#[tokio::test]
#[serial]
async fn a_full_resync_of_a_sparse_group_adds_nothing() {
    let fx = setup("resync-sparse").await;
    let batch = sparse_group(T1, &[(1, 76.0), (2, 59.0)]);

    let body = ingest(&fx, batch.clone()).await;
    assert_eq!(body["inserted"], 2, "{body}");
    assert_eq!(stored_indexes(&fx).await, vec![1, 2]);
    let (mean, n) = sample_mean_and_n(&fx).await;
    assert!((mean - 67.5).abs() < 1e-9, "mean of 76 and 59: {mean}");
    assert_eq!(n, 2);

    let body = ingest(&fx, batch).await;
    assert_eq!(body["inserted"], 0, "the replay is a no-op: {body}");
    assert_eq!(
        stored_indexes(&fx).await,
        vec![1, 2],
        "the replay found its own keys already present"
    );
    let (mean, n) = sample_mean_and_n(&fx).await;
    assert!((mean - 67.5).abs() < 1e-9, "the mean is unmoved: {mean}");
    assert_eq!(n, 2);
}
