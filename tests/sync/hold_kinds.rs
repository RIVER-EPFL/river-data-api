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
    db.query_one_raw(Statement::from_string(
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
    db.execute_raw(Statement::from_string(
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
        .query_one_raw(Statement::from_string(
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

/// Scenario: one instant both disagrees statistically with the source and carries a curated row
/// the source changed. Two writers, two different kinds, one `(stream, instant)`.
///
/// Expected behaviour: both holds stand. The statistics hold keeps the `{index, value}` pairs the
/// `flag` resolution addresses replicates by, and the `source_modified` hold keeps its own
/// evidence and label; neither overwrites the other, and the review UI branches on a kind that is
/// still true.
#[tokio::test]
#[serial]
async fn two_kinds_coexist_at_one_instant() {
    let (db, app, token) = setup().await;
    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({"source_system": "cnet", "source_key": "stn:coexist:reps", "measurement_type": "spot"}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "{stream}");
    let stream_id = crate::common::e2e::id_of(&stream);

    let stats = json!({
        "mean": 1.5, "sd": 0.1, "n": 3,
        "values": [{"index": 0, "value": 1.0}, {"index": 1, "value": 2.0}],
    });
    db.execute_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "INSERT INTO replicate_audit_holds \
                 (stream_id, group_time, kind, expected, computed, delta, status) \
             VALUES ('{stream_id}', '{T1}', 'replicate_stats', \
                     '{{\"mean\": 1.0, \"sd\": 0.1, \"n\": 3}}', '{stats}', '{{}}', 'pending')"
        ),
    ))
    .await
    .unwrap();

    db.execute_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "INSERT INTO replicate_audit_holds \
                 (stream_id, group_time, kind, expected, computed, delta, status) \
             VALUES ('{stream_id}', '{T1}', 'source_modified', \
                     '{{\"claim\": \"replaced\"}}', '{{\"kept\": true}}', '{{}}', 'pending') \
             ON CONFLICT (stream_id, group_time, kind) WHERE status IN ('pending', 'deferred') \
             DO UPDATE SET expected = EXCLUDED.expected, computed = EXCLUDED.computed"
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
    let holds = body["holds"].as_array().unwrap();
    let kinds: Vec<&str> = holds.iter().filter_map(|h| h["kind"].as_str()).collect();
    assert!(
        kinds.contains(&"replicate_stats") && kinds.contains(&"source_modified"),
        "both kinds stand at the instant: {kinds:?}"
    );

    let stats_hold = holds
        .iter()
        .find(|h| h["kind"] == "replicate_stats")
        .expect("the statistics hold survives");
    assert!(
        stats_hold["computed"]["values"].is_array(),
        "its replicate values survive, so `flag` can still address them: {stats_hold}"
    );
}

/// Scenario: a windowed pass braked and its ruling is waiting in the queue; the next cycle's
/// statistics audit agrees with the source at the same instant.
///
/// Expected behaviour: the brake hold still stands. An agreeing re-audit supersedes the statistics
/// hold it is about and nothing else; closing a brake ruling nobody acted on would release a pass
/// the operator never admitted.
#[tokio::test]
#[serial]
async fn an_agreeing_re_audit_leaves_a_standing_brake_alone() {
    let (db, app, token) = setup().await;
    let (sync_token, _service) = crate::common::seed_sync_session_token(&db).await;
    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({"source_system": "cnet", "source_key": "stn:brake:reps", "measurement_type": "spot"}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "{stream}");
    let stream_id = crate::common::e2e::id_of(&stream);
    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({"site_parameter_id": crate::common::PARAM_S1_TEMP_ID}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "pair: {body}");

    db.execute_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "INSERT INTO replicate_audit_holds \
                 (stream_id, group_time, kind, expected, computed, delta, status) \
             VALUES ('{stream_id}', '{T1}', 'brake_fired', \
                     '{{\"would_withdraw\": 40}}', '{{\"held\": \"changed and withdrawn\"}}', \
                     '{{}}', 'pending')"
        ),
    ))
    .await
    .unwrap();

    // 10, 12, 14: mean 12, sample sd 2. The source states exactly that, so the audit agrees.
    let readings: Vec<serde_json::Value> = [10.0, 12.0, 14.0]
        .iter()
        .enumerate()
        .map(|(i, v)| json!({"time": T1, "raw_value": v, "replicate_index": i}))
        .collect();
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &json!({
            "stream_id": stream_id,
            "readings": readings,
            "audit": [{"time": T1, "expected_mean": 12.0, "expected_sd": 2.0, "expected_n": 3}],
        }),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "ingest: {body}");

    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT status FROM replicate_audit_holds \
                 WHERE stream_id = '{stream_id}' AND group_time = '{T1}' AND kind = 'brake_fired'"
            ),
        ))
        .await
        .unwrap()
        .expect("the brake hold is still there");
    assert_eq!(
        row.try_get::<String>("", "status").unwrap(),
        "pending",
        "an agreeing statistics audit must not close a brake ruling"
    );
}

/// Expected behaviour: the list reports pending holds per kind, so the audits tab, its banner and
/// the operations badge can name what is waiting instead of calling every kind a
/// replicate-statistics disagreement.
#[tokio::test]
#[serial]
async fn the_list_breaks_pending_holds_down_by_kind() {
    let (db, app, token) = setup().await;
    insert_event_finding(&db).await;
    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({"source_system": "cnet", "source_key": "stn:kinds:reps", "measurement_type": "spot"}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "{stream}");
    let stream_id = crate::common::e2e::id_of(&stream);
    for (kind, at) in [
        ("replicate_stats", "2025-06-02T08:00:00Z"),
        ("replicate_stats", "2025-06-03T08:00:00Z"),
        ("brake_fired", "2025-06-04T08:00:00Z"),
    ] {
        db.execute_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "INSERT INTO replicate_audit_holds                      (stream_id, group_time, kind, expected, computed, delta, status)                  VALUES ('{stream_id}', '{at}', '{kind}', '{{}}', '{{}}', '{{}}', 'pending')"
            ),
        ))
        .await
        .unwrap();
    }

    let (status, body) = crate::common::get_json_with_token(
        &app,
        "/api/sync/replicate_audit_holds?status=pending",
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["pending"], 4, "four pending in total: {body}");
    let by_kind = &body["pending_by_kind"];
    assert_eq!(by_kind["replicate_stats"], 2, "{by_kind}");
    assert_eq!(by_kind["brake_fired"], 1, "{by_kind}");
    assert_eq!(
        by_kind["stale_output"], 1,
        "the event finding counts too: {by_kind}"
    );
    assert!(
        by_kind.get("source_modified").is_none(),
        "a kind with nothing pending is absent, not zero: {by_kind}"
    );
}
