//! Resolving an audit hold by flagging names replicate indexes, not positions in the hold's value
//! list. A family whose source omitted NULL cells has gaps in its indexes, so the two differ, and
//! an index the group does not hold flags nothing rather than half-applying.
//!
//! Run: cargo test --test sync replicate_flag_indexes -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

const T1: &str = "2025-06-01T08:00:00Z";

struct Fixture {
    db: DatabaseConnection,
    app: axum::Router,
    token: String,
    sync_token: String,
    stream: String,
}

/// A family at indexes 0, 1, 3 and 4 whose audited mean disagrees, so a pending hold exists.
async fn setup_gapped_group(key: &str) -> (Fixture, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (sync_token, _service_id) = crate::common::seed_sync_session_token(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({"source_system": "gapsrc", "source_key": key, "measurement_type": "spot"}),
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

    let fx = Fixture {
        db,
        app,
        token,
        sync_token,
        stream,
    };

    let readings: Vec<serde_json::Value> = [(0, 10.0), (1, 20.0), (3, 30.0), (4, 999.0)]
        .iter()
        .map(|(i, v)| json!({"time": T1, "raw_value": v, "replicate_index": i}))
        .collect();
    let (status, body) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/ingest",
        &json!({
            "stream_id": fx.stream,
            "readings": readings,
            "audit": [{"time": T1, "expected_mean": 20.0,
                       "expected_sd": 10.0, "expected_n": 4}],
        }),
        &fx.sync_token,
    )
    .await;
    assert_eq!(status, 200, "audited ingest ({status}): {body}");

    let (status, holds) = crate::common::get_json_with_token(
        &fx.app,
        &format!("/api/sync/replicate_audit_holds?stream_id={}", fx.stream),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "list holds ({status}): {holds}");
    assert_eq!(holds["pending"], 1, "the disagreement holds: {holds}");
    let hold_id = holds["holds"][0]["id"].as_str().unwrap().to_string();
    (fx, hold_id)
}

async fn flagged_indexes(fx: &Fixture) -> Vec<i16> {
    fx.db
        .query_all(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT replicate_index FROM readings \
                 WHERE stream_id = '{}' AND time = '{T1}' AND is_flagged IS TRUE \
                 ORDER BY replicate_index",
                fx.stream
            ),
        ))
        .await
        .unwrap()
        .iter()
        .map(|row| row.try_get::<i16>("", "replicate_index").unwrap())
        .collect()
}

async fn hold_status(fx: &Fixture, hold_id: &str) -> String {
    fx.db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT status FROM replicate_audit_holds WHERE id = '{hold_id}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<String>("", "status")
        .unwrap()
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

async fn resolve(
    fx: &Fixture,
    hold_id: &str,
    body: &serde_json::Value,
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        &fx.app,
        &format!("/api/sync/replicate_audit_holds/{hold_id}/resolve"),
        body,
        &fx.token,
    )
    .await
}

#[tokio::test]
#[serial]
async fn flagging_index_three_flags_the_reading_at_index_three() {
    let (fx, hold_id) = setup_gapped_group("gap-flag").await;

    let (status, body) = resolve(
        &fx,
        &hold_id,
        &json!({"mode": "flag", "replicate_indexes": [3]}),
    )
    .await;
    assert_eq!(status, 200, "resolve flag ({status}): {body}");

    assert_eq!(
        flagged_indexes(&fx).await,
        vec![3],
        "the third position in the hold's value list is index 3, not the flagged one"
    );

    let (mean, n) = sample_mean_and_n(&fx).await;
    assert_eq!(n, 3);
    assert!(
        (mean - 343.0).abs() < 1e-9,
        "mean of 10, 20 and 999: {mean}"
    );
}

#[tokio::test]
#[serial]
async fn an_index_the_group_does_not_hold_flags_nothing() {
    let (fx, hold_id) = setup_gapped_group("gap-closed").await;

    let (status, body) = resolve(
        &fx,
        &hold_id,
        &json!({"mode": "flag", "replicate_indexes": [3, 7]}),
    )
    .await;
    assert_eq!(status, 400, "resolve ({status}): {body}");
    assert!(
        flagged_indexes(&fx).await.is_empty(),
        "the whole resolution rolled back, index 3 included"
    );
    assert_eq!(hold_status(&fx, &hold_id).await, "pending");

    let (mean, n) = sample_mean_and_n(&fx).await;
    assert_eq!(n, 4);
    assert!(
        (mean - 264.75).abs() < 1e-9,
        "mean of the untouched group: {mean}"
    );
}
