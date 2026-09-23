//! Scenario: a manager rejects an intern's entry at a manual visit a calculation reads, or reopens
//! that ruling, and the visit's `event_recompute` cannot be queued.
//!
//! Expected behaviour: the recompute is queued with the ruling, so a refused enqueue leaves the
//! hold as it was and a retry lands.
//!
//! Run: cargo test --test sync ruling_visit_recompute -- --test-threads=1

use river_db::common::bulk_write;
use river_db::routes::private::readings::models::{Kind, Origin};
use river_db::routes::private::readings::service::{self as decisions, Decision, DecisionKey};
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
    event: Uuid,
    hold: Uuid,
}

/// An intern's pending entry at a manual visit the seeded calculation reads, and its hold.
async fn setup() -> Fixture {
    let f = crate::common::seeded_app().await;
    crate::common::seed_visit_calculation(&f.db, "ruling_visit_input", "DO_Temperature").await;
    let stream = crate::common::sensor_lifecycle::create_paired_stream(
        &f.db,
        "ruling-visit",
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
    let entry = Decision {
        key: DecisionKey {
            stream_id: stream,
            time: AT.parse().expect("instant"),
            replicate_index: None,
        },
        kind: Kind::UnverifiedEntry,
        new: json!({}),
        actor: "intern".to_string(),
        reason: Some("entered".to_string()),
        origin: Origin::Manual,
        set_id: None,
    };
    bulk_write::guarded(&f.db, async |txn| decisions::record(txn, &entry).await)
        .await
        .expect("the entry is pending");
    let hold = Uuid::new_v4();
    exec(
        &f.db,
        &format!(
            "INSERT INTO replicate_audit_holds \
                 (id, stream_id, site_id, parameter_id, group_time, kind, expected, computed, \
                  delta, status) \
             VALUES ('{hold}', NULL, '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{AT}', \
                     'unverified_entry', '{{\"state\": \"verified\"}}'::jsonb, \
                     '{{\"state\": \"unverified\"}}'::jsonb, '{{}}'::jsonb, 'pending')"
        ),
    )
    .await;
    Fixture {
        app: f.app,
        token: f.token,
        db: f.db,
        event,
        hold,
    }
}

async fn post(f: &Fixture, action: &str, body: &serde_json::Value) -> (u16, String) {
    crate::common::post_json_with_token(
        &f.app,
        &format!("/api/sync/replicate_audit_holds/{}/{action}", f.hold),
        body,
        &f.token,
    )
    .await
}

async fn reject(f: &Fixture) -> (u16, String) {
    post(f, "resolve", &json!({ "mode": "reject" })).await
}

async fn reopen(f: &Fixture) -> (u16, String) {
    post(f, "reopen", &json!({})).await
}

async fn pending(f: &Fixture) -> i64 {
    e2e::count(
        &f.db,
        &format!(
            "SELECT COUNT(*)::bigint FROM replicate_audit_holds \
             WHERE id = '{}' AND status = 'pending'",
            f.hold
        ),
    )
    .await
}

async fn withdrawn(f: &Fixture) -> i64 {
    e2e::count(
        &f.db,
        &format!(
            "SELECT COUNT(*)::bigint FROM readings \
             WHERE collection_event_id = '{}' AND withdrawn_at IS NOT NULL",
            f.event
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

#[tokio::test]
#[serial]
async fn a_ruling_whose_visit_recompute_cannot_be_queued_rules_nothing() {
    let f = setup().await;

    crate::common::jobs::refuse_enqueue(&f.db, "event_recompute").await;
    let (status, body) = reject(&f).await;
    crate::common::jobs::restore_enqueue(&f.db).await;

    assert_eq!(status, 500, "the ruling reports the failure: {body}");
    assert_eq!(pending(&f).await, 1, "the hold is still pending");
    assert_eq!(withdrawn(&f).await, 0, "the entry still stands");
    let (status, body) = reject(&f).await;
    assert_eq!(status, 200, "a retry rules on the entry: {body}");
    assert_eq!(withdrawn(&f).await, 1);
    assert_eq!(recomputes_queued(&f).await, 1);
}

#[tokio::test]
#[serial]
async fn a_reopen_whose_visit_recompute_cannot_be_queued_reopens_nothing() {
    let f = setup().await;
    let (status, body) = reject(&f).await;
    assert_eq!(status, 200, "reject: {body}");
    exec(
        &f.db,
        &format!(
            "UPDATE reprocessing_jobs SET status = 'completed', dedupe_key = NULL \
             WHERE trigger_type = 'event_recompute' AND trigger_id = '{}'",
            f.event
        ),
    )
    .await;

    crate::common::jobs::refuse_enqueue(&f.db, "event_recompute").await;
    let (status, body) = reopen(&f).await;
    crate::common::jobs::restore_enqueue(&f.db).await;

    assert_eq!(status, 500, "the reopen reports the failure: {body}");
    assert_eq!(pending(&f).await, 0, "the ruling still stands");
    assert_eq!(withdrawn(&f).await, 1, "the entry is still withdrawn");
    let (status, body) = reopen(&f).await;
    assert_eq!(status, 200, "a retry reopens the ruling: {body}");
    assert_eq!(pending(&f).await, 1);
    assert_eq!(withdrawn(&f).await, 0);
    assert_eq!(recomputes_queued(&f).await, 2);
}
