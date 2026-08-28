//! The review queue lists every hold kind: replicate-statistics holds on streams and event-audit
//! findings keyed on (site, parameter, instant) with no stream at all.
//!
//! Run: cargo test --test sync hold_kinds -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::{GLOBAL_PARAM_DO_ID, SITE1_ID};

const T1: &str = "2025-06-01T08:00:00Z";

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

async fn insert_event_finding(db: &DatabaseConnection) -> String {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "INSERT INTO replicate_audit_holds \
                 (kind, site_id, parameter_id, group_time, tool, expected, computed, delta, status) \
             VALUES ('stale_output', '{SITE1_ID}', '{GLOBAL_PARAM_DO_ID}', '{T1}', 'chain_b', \
                     '{{\"value\": 55.0}}', '{{\"value\": 47.0}}', '{{}}', 'pending') \
             RETURNING id::text AS id"
        ),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get("", "id")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn an_event_finding_lists_with_its_kind_and_no_stream() {
    let (db, app, token) = setup().await;
    let hold_id = insert_event_finding(&db).await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        "/api/sync/replicate_audit_holds?status=pending",
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let holds = body["holds"].as_array().unwrap();
    let finding = holds
        .iter()
        .find(|h| h["id"] == hold_id.as_str())
        .expect("the event finding is in the queue");
    assert_eq!(finding["kind"], "stale_output");
    assert!(finding["stream_id"].is_null());
    assert_eq!(finding["tool"], "chain_b");
    assert!(
        finding["site_name"].is_string() && finding["parameter_name"].is_string(),
        "the slot names resolve from the finding's own site and parameter: {finding}"
    );
    assert_eq!(finding["paired"], false);
}

#[tokio::test]
#[serial]
async fn a_stream_hold_still_lists_and_carries_its_kind() {
    let (db, app, token) = setup().await;
    let (_sync_token, _service) = crate::common::seed_sync_session_token(&db).await;
    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({"source_system": "cnet", "source_key": "stn:x:reps", "measurement_type": "spot"}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "{stream}");
    let stream_id = crate::common::e2e::id_of(&stream);
    db.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "INSERT INTO replicate_audit_holds (stream_id, group_time, expected, computed, delta) \
             VALUES ('{stream_id}', '{T1}', '{{\"mean\": 1.0, \"sd\": 0.1, \"n\": 3}}', \
                     '{{\"mean\": 1.5, \"sd\": 0.1, \"n\": 3}}', '{{\"mean\": 0.5, \"sd\": 0.0}}')"
        ),
    ))
    .await
    .unwrap();

    let (status, body) = crate::common::get_json_with_token(
        &app,
        "/api/sync/replicate_audit_holds?status=pending",
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let hold = &body["holds"][0];
    assert_eq!(hold["kind"], "replicate_stats");
    assert_eq!(hold["stream_id"], stream_id.as_str());
    assert_eq!(hold["source_system"], "cnet");
}

#[tokio::test]
#[serial]
async fn an_event_finding_can_be_acknowledged() {
    let (db, app, token) = setup().await;
    let hold_id = insert_event_finding(&db).await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sync/replicate_audit_holds/{hold_id}/acknowledge"),
        &json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let status_now: String = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT status FROM replicate_audit_holds WHERE id = '{hold_id}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "status")
        .unwrap();
    assert_eq!(status_now, "acknowledged");
}
