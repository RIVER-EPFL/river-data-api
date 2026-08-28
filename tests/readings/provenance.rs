//! The provenance resolver: one instant of one series, addressed by stream or by slot, answered
//! with the assembled record (origin, per-replicate corrections, event, computation, state).
//!
//! Run: cargo test --test readings provenance -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::{GLOBAL_PARAM_DO_ID, PARAM_S1_DO_ID, SITE1_ID};

const T1: &str = "2025-06-01T08:00:00Z";

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

fn slot_uri(time: &str) -> String {
    format!(
        "/api/readings/provenance?site_id={SITE1_ID}&parameter_id={GLOBAL_PARAM_DO_ID}&time={}",
        time.replace('+', "%2B")
    )
}

async fn save_grab(app: &axum::Router, token: &str) {
    let (status, body) = crate::common::post_json_with_token(
        app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "readings": [
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 10.0, "time": T1 },
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 12.0, "time": T1 },
            ],
        }),
        token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
#[serial]
async fn the_slot_form_assembles_a_grab_instant() {
    let (_db, app, token) = setup().await;
    save_grab(&app, &token).await;

    let (status, body) =
        crate::common::get_json_with_token(&app, &slot_uri(T1), &token).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["duplicate_slot"], false);
    let records = body["records"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    let rec = &records[0];
    assert_eq!(rec["origin"]["classification"], "manual");
    assert_eq!(rec["origin"]["source_system"], "grab_sample");
    assert!(
        rec["origin"]["ingested_at"].is_string(),
        "a fresh insert carries its arrival stamp: {rec}"
    );
    assert_eq!(rec["readings"].as_array().unwrap().len(), 2);
    assert_eq!(rec["event"]["source"], "manual");
    assert!(
        rec["computation"]["sample_id"].is_string(),
        "the replicate group's sample is the computation handle: {rec}"
    );

    // The stream form answers identically for the same rows.
    let stream_id = rec["origin"]["stream_id"].as_str().unwrap();
    let (status, by_stream) = crate::common::get_json_with_token(
        &app,
        &format!("/api/readings/provenance?stream_id={stream_id}&time={T1}"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{by_stream}");
    assert_eq!(by_stream["records"][0]["readings"], rec["readings"]);
}

#[tokio::test]
#[serial]
async fn flag_and_withdrawal_state_travel_on_the_facets() {
    let (db, app, token) = setup().await;
    save_grab(&app, &token).await;

    db.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "UPDATE readings SET is_flagged = TRUE, flag_reason = 'outlier' \
             WHERE site_id = '{SITE1_ID}' AND time = '{T1}' AND replicate_index = 0"
        ),
    ))
    .await
    .unwrap();
    db.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "UPDATE readings SET withdrawn_at = NOW(), withdrawn_reason = 'absent from window' \
             WHERE site_id = '{SITE1_ID}' AND time = '{T1}' AND replicate_index = 1"
        ),
    ))
    .await
    .unwrap();

    let (status, body) =
        crate::common::get_json_with_token(&app, &slot_uri(T1), &token).await;
    assert_eq!(status, 200, "{body}");
    let readings = body["records"][0]["readings"].as_array().unwrap();
    assert_eq!(readings[0]["is_flagged"], true);
    assert_eq!(readings[0]["flag_reason"], "outlier");
    assert!(readings[1]["withdrawn_at"].is_string());
    assert_eq!(readings[1]["withdrawn_reason"], "absent from window");
}

#[tokio::test]
#[serial]
async fn two_streams_on_one_slot_report_duplicate_slot() {
    let (db, app, token) = setup().await;
    let (sync_token, _service) = crate::common::seed_sync_session_token(&db).await;

    for key in ["stn:a", "stn:b"] {
        let (status, stream) = crate::common::post_json_parse_with_token(
            &app,
            "/api/streams/register",
            &json!({"source_system": "cnet", "source_key": key, "measurement_type": "spot"}),
            &token,
        )
        .await;
        assert!((200..300).contains(&status), "{stream}");
        let stream_id = crate::common::e2e::id_of(&stream);
        let (status, body) = crate::common::post_json_with_token(
            &app,
            &format!("/api/streams/{stream_id}/pair"),
            &json!({"site_parameter_id": PARAM_S1_DO_ID}),
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let (status, body) = crate::common::post_json_with_token(
            &app,
            "/api/ingest",
            &json!({
                "stream_id": stream_id,
                "readings": [{ "time": T1, "raw_value": 5.0, "replicate_index": 0 }],
            }),
            &sync_token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
    }

    let (status, body) =
        crate::common::get_json_with_token(&app, &slot_uri(T1), &token).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["duplicate_slot"], true);
    assert_eq!(body["records"].as_array().unwrap().len(), 2);
    for rec in body["records"].as_array().unwrap() {
        assert_eq!(rec["origin"]["classification"], "sync");
    }
}

#[tokio::test]
#[serial]
async fn holds_touching_the_instant_are_listed() {
    let (db, app, token) = setup().await;
    save_grab(&app, &token).await;

    db.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "INSERT INTO replicate_audit_holds \
                 (kind, site_id, parameter_id, group_time, tool, expected, computed, delta, status) \
             VALUES ('stale_output', '{SITE1_ID}', '{GLOBAL_PARAM_DO_ID}', '{T1}', 'chain_b', \
                     '{{\"value\": 55.0}}', '{{\"value\": 47.0}}', '{{}}', 'pending')"
        ),
    ))
    .await
    .unwrap();

    let (status, body) =
        crate::common::get_json_with_token(&app, &slot_uri(T1), &token).await;
    assert_eq!(status, 200, "{body}");
    let holds = body["records"][0]["holds"].as_array().unwrap();
    assert_eq!(holds.len(), 1, "{body}");
    assert_eq!(holds[0]["kind"], "stale_output");
    assert_eq!(holds[0]["status"], "pending");
}

#[tokio::test]
#[serial]
async fn missing_instant_and_missing_key_are_refused() {
    let (_db, app, token) = setup().await;

    let (status, _) =
        crate::common::get_json_with_token(&app, &slot_uri("2030-01-01T00:00:00Z"), &token).await;
    assert_eq!(status, 404);

    let (status, _) = crate::common::get_json_with_token(
        &app,
        &format!("/api/readings/provenance?time={T1}"),
        &token,
    )
    .await;
    assert_eq!(status, 400);
}
