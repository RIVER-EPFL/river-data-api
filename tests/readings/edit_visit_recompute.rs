//! Scenario: a curation edit moves a spot value at a manual visit a calculation reads, so it owes
//! the visit an `event_recompute`, and that recompute cannot be queued.
//!
//! Expected behaviour: the answer matches whether the change committed. An edit and its rollbacks
//! queue the recompute with the decision, so a refused enqueue records nothing and a retry lands;
//! the flag route reports the lost enqueue and answers for the flag it recorded.
//!
//! Run: cargo test --test readings edit_visit_recompute -- --test-threads=1

use sea_orm::DatabaseConnection;
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_TEMP_ID, PARAM_S1_TEMP_ID, SITE1_ID, e2e, exec};

const AT: &str = "2025-10-06T09:00:00Z";

struct Fixture {
    app: axum::Router,
    token: String,
    db: DatabaseConnection,
    stream: Uuid,
    event: Uuid,
}

async fn setup() -> Fixture {
    let f = crate::common::seeded_app().await;
    crate::common::seed_visit_calculation(&f.db, "edit_visit_input", "DO_Temperature").await;
    let stream = crate::common::sensor_lifecycle::create_paired_stream(
        &f.db,
        "edit-visit",
        PARAM_S1_TEMP_ID,
    )
    .await;
    let event = Uuid::new_v4();
    exec(
        &f.db,
        &format!(
            "INSERT INTO collection_events (id, site_id, collected_at, source) \
             VALUES ('{event}', '{SITE1_ID}', '{AT}', 'manual')"
        ),
    )
    .await;
    exec(
        &f.db,
        &format!(
            "INSERT INTO readings \
                 (stream_id, site_id, parameter_id, time, raw_value, replicate_index, \
                  measurement_type, collection_event_id) \
             VALUES ('{stream}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{AT}', 10, 0, 'spot', \
                     '{event}')"
        ),
    )
    .await;
    Fixture {
        app: f.app,
        token: f.token,
        db: f.db,
        stream,
        event,
    }
}

fn selection(f: &Fixture) -> serde_json::Value {
    json!({ "keys": [{ "stream_id": f.stream, "time": AT, "replicate_index": 0 }] })
}

fn flag() -> serde_json::Value {
    json!({ "kind": "flag", "reason": "spike" })
}

async fn commit_edit(f: &Fixture) -> (u16, serde_json::Value) {
    let (status, preview) = crate::common::post_json_parse_with_token(
        &f.app,
        "/api/readings/edits/preview",
        &json!({ "selection": selection(f), "decision": flag() }),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "preview: {preview}");
    crate::common::post_json_parse_with_token(
        &f.app,
        "/api/readings/edits",
        &json!({
            "selection": selection(f),
            "decision": flag(),
            "preview_id": preview["preview_id"].as_str().expect("preview id"),
        }),
        &f.token,
    )
    .await
}

async fn flagged(f: &Fixture) -> i64 {
    e2e::count(
        &f.db,
        &format!(
            "SELECT COUNT(*)::bigint FROM readings \
             WHERE stream_id = '{}' AND is_flagged IS TRUE",
            f.stream
        ),
    )
    .await
}

async fn recomputes_queued(f: &Fixture) -> i64 {
    e2e::count(
        &f.db,
        &format!(
            "SELECT COUNT(*)::bigint FROM reprocessing_jobs \
             WHERE trigger_type = 'event_recompute' AND trigger_id = '{}'",
            f.event
        ),
    )
    .await
}

/// Finish the visit's queued recompute, so the next write queues a row of its own rather than
/// coalescing onto the first by its dedupe key.
async fn settle_recomputes(f: &Fixture) {
    exec(
        &f.db,
        &format!(
            "UPDATE reprocessing_jobs SET status = 'completed', dedupe_key = NULL \
             WHERE trigger_type = 'event_recompute' AND trigger_id = '{}'",
            f.event
        ),
    )
    .await;
}

/// A committed flag edit, its recompute settled, as (decision id, set id).
async fn committed_flag(f: &Fixture) -> (String, String) {
    let (status, committed) = commit_edit(f).await;
    assert_eq!(status, 200, "commit: {committed}");
    settle_recomputes(f).await;
    (
        committed["decision_ids"][0]
            .as_str()
            .expect("decision id")
            .to_string(),
        committed["set_id"].as_str().expect("set id").to_string(),
    )
}

#[tokio::test]
#[serial]
async fn an_edit_whose_visit_recompute_cannot_be_queued_records_nothing() {
    let f = setup().await;

    crate::common::jobs::refuse_enqueue(&f.db, "event_recompute").await;
    let (status, body) = commit_edit(&f).await;
    crate::common::jobs::restore_enqueue(&f.db).await;

    assert_eq!(status, 500, "the edit reports the failure: {body}");
    assert_eq!(flagged(&f).await, 0, "the flag was not recorded");
    let (status, body) = commit_edit(&f).await;
    assert_eq!(status, 200, "a retry records the edit: {body}");
    assert_eq!(flagged(&f).await, 1);
    assert_eq!(recomputes_queued(&f).await, 1);
}

#[tokio::test]
#[serial]
async fn a_rollback_whose_visit_recompute_cannot_be_queued_inverts_nothing() {
    let f = setup().await;
    let (decision, _) = committed_flag(&f).await;
    let rollback = format!("/api/readings/edits/{decision}/rollback");

    crate::common::jobs::refuse_enqueue(&f.db, "event_recompute").await;
    let (status, body) =
        crate::common::post_json_parse_with_token(&f.app, &rollback, &json!({}), &f.token).await;
    crate::common::jobs::restore_enqueue(&f.db).await;

    assert_eq!(status, 500, "the rollback reports the failure: {body}");
    assert_eq!(flagged(&f).await, 1, "the flag still stands");
    let (status, body) =
        crate::common::post_json_parse_with_token(&f.app, &rollback, &json!({}), &f.token).await;
    assert_eq!(status, 200, "a retry rolls the edit back: {body}");
    assert_eq!(flagged(&f).await, 0);
    assert_eq!(recomputes_queued(&f).await, 2);
}

#[tokio::test]
#[serial]
async fn a_set_rollback_whose_visit_recompute_cannot_be_queued_inverts_nothing() {
    let f = setup().await;
    let (_, set) = committed_flag(&f).await;
    let rollback = format!("/api/readings/edits/sets/{set}/rollback");

    crate::common::jobs::refuse_enqueue(&f.db, "event_recompute").await;
    let (status, body) =
        crate::common::post_json_parse_with_token(&f.app, &rollback, &json!({}), &f.token).await;
    crate::common::jobs::restore_enqueue(&f.db).await;

    assert_eq!(status, 500, "the rollback reports the failure: {body}");
    assert_eq!(flagged(&f).await, 1, "the flag still stands");
    let (status, body) =
        crate::common::post_json_parse_with_token(&f.app, &rollback, &json!({}), &f.token).await;
    assert_eq!(status, 200, "a retry rolls the set back: {body}");
    assert_eq!(flagged(&f).await, 0);
    assert_eq!(recomputes_queued(&f).await, 2);
}

#[tokio::test]
#[serial]
async fn a_flag_whose_visit_recompute_cannot_be_queued_answers_for_the_flag_it_recorded() {
    let f = setup().await;

    crate::common::jobs::refuse_enqueue(&f.db, "event_recompute").await;
    let (status, body) = crate::common::patch_json_parse_with_token(
        &f.app,
        "/api/readings/flag",
        &json!({
            "readings": [{
                "site_id": SITE1_ID,
                "parameter_id": GLOBAL_PARAM_TEMP_ID,
                "time": AT,
                "replicate_index": 0,
            }],
            "reason": "spike",
        }),
        &f.token,
    )
    .await;
    crate::common::jobs::restore_enqueue(&f.db).await;

    assert_eq!(status, 200, "the flag is recorded and says so: {body}");
    assert_eq!(flagged(&f).await, 1);
}
