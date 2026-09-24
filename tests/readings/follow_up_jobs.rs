//! Scenario: a write whose follow-up job (`derived_recompute`, `alarm_backfill`, `batch_derived`,
//! `ingest_derived`, the pin reprocess) cannot be queued, or whose rollup refresh fails.
//!
//! Expected behaviour: the answer matches whether the change committed. A follow-up job is queued
//! in the writer's own transaction, so a refused enqueue writes nothing and a retry succeeds; a
//! failed refresh leaves a committed write answering 200, since the hourly policy rematerialises
//! the span regardless.
//!
//! Run: cargo test --test readings follow_up_jobs -- --test-threads=1

use sea_orm::DatabaseConnection;
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::jobs::{refuse_enqueue, refuse_refresh, restore_enqueue, restore_refresh};
use crate::common::{GLOBAL_PARAM_TEMP_ID, PARAM_S1_TEMP_ID, SITE1_ID};

const AT: &str = "2025-06-15T10:04:00Z";

struct Fixture {
    db: DatabaseConnection,
    app: axum::Router,
    token: String,
}

async fn setup() -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app_without_worker(db.clone());
    Fixture { db, app, token }
}

/// One continuous temperature reading at `AT` on a paired stream. Returns the stream.
async fn continuous_reading(db: &DatabaseConnection) -> Uuid {
    let stream =
        crate::common::sensor_lifecycle::create_paired_stream(db, "follow-up", PARAM_S1_TEMP_ID)
            .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings \
                 (stream_id, site_id, parameter_id, time, raw_value, calibrated_value, replicate_index) \
             VALUES ('{stream}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{AT}', 50, 50, 0)"
        ),
    )
    .await;
    stream
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    crate::common::e2e::count(db, sql).await
}

async fn queued(db: &DatabaseConnection, trigger_type: &str) -> i64 {
    count(
        db,
        &format!(
            "SELECT COUNT(*)::bigint FROM reprocessing_jobs WHERE trigger_type = '{trigger_type}'"
        ),
    )
    .await
}

async fn flagged(db: &DatabaseConnection, stream: Uuid) -> i64 {
    count(
        db,
        &format!(
            "SELECT COUNT(*)::bigint FROM readings \
             WHERE stream_id = '{stream}' AND is_flagged IS TRUE"
        ),
    )
    .await
}

async fn flag(fx: &Fixture) -> (u16, String) {
    crate::common::patch_json_with_token(
        &fx.app,
        "/api/readings/flag",
        &json!({
            "readings": [{ "site_id": SITE1_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": AT }],
            "reason": "spike"
        }),
        &fx.token,
    )
    .await
}

async fn flag_range(fx: &Fixture) -> (u16, String) {
    crate::common::patch_json_with_token(
        &fx.app,
        "/api/readings/flag_range",
        &json!({
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "start_time": "2025-06-15T10:00:00Z",
            "end_time": "2025-06-15T10:59:00Z",
            "reason": "spike"
        }),
        &fx.token,
    )
    .await
}

#[tokio::test]
#[serial]
async fn a_flag_whose_derived_recompute_cannot_be_queued_flags_nothing() {
    let fx = setup().await;
    let stream = continuous_reading(&fx.db).await;

    refuse_enqueue(&fx.db, "derived_recompute").await;
    let (status, body) = flag(&fx).await;
    restore_enqueue(&fx.db).await;

    assert_ne!(status, 200, "the flag reports the failure: {body}");
    assert_eq!(flagged(&fx.db, stream).await, 0, "and records nothing");

    let (status, body) = flag(&fx).await;
    assert_eq!(status, 200, "a retry flags the reading: {body}");
    assert_eq!(flagged(&fx.db, stream).await, 1);
    assert_eq!(queued(&fx.db, "derived_recompute").await, 1);

    crate::common::cleanup_test_db(&fx.db).await;
}

#[tokio::test]
#[serial]
async fn a_range_flag_whose_derived_recompute_cannot_be_queued_flags_nothing() {
    let fx = setup().await;
    let stream = continuous_reading(&fx.db).await;

    refuse_enqueue(&fx.db, "derived_recompute").await;
    let (status, body) = flag_range(&fx).await;
    restore_enqueue(&fx.db).await;

    assert_ne!(status, 200, "the range flag reports the failure: {body}");
    assert_eq!(flagged(&fx.db, stream).await, 0, "and records nothing");

    let (status, body) = flag_range(&fx).await;
    assert_eq!(status, 200, "a retry flags the range: {body}");
    assert_eq!(flagged(&fx.db, stream).await, 1);
    assert_eq!(queued(&fx.db, "derived_recompute").await, 1);

    crate::common::cleanup_test_db(&fx.db).await;
}

#[tokio::test]
#[serial]
async fn a_flag_whose_rollup_refresh_fails_answers_the_flag_it_recorded() {
    let fx = setup().await;
    let stream = continuous_reading(&fx.db).await;

    refuse_refresh(&fx.db).await;
    let (status, body) = flag(&fx).await;
    restore_refresh(&fx.db).await;

    assert_eq!(status, 200, "the flag is committed: {body}");
    assert_eq!(flagged(&fx.db, stream).await, 1);

    crate::common::cleanup_test_db(&fx.db).await;
}

async fn batch(fx: &Fixture) -> (u16, String) {
    crate::common::post_json_with_token(
        &fx.app,
        "/api/readings/batch",
        &json!({
            "readings": [{
                "site_id": SITE1_ID,
                "parameter_id": GLOBAL_PARAM_TEMP_ID,
                "time": AT,
                "raw_value": 12.5,
            }]
        }),
        &fx.token,
    )
    .await
}

async fn stored_at(db: &DatabaseConnection, at: &str) -> i64 {
    count(
        db,
        &format!(
            "SELECT COUNT(*)::bigint FROM readings \
             WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}' \
               AND time = '{at}'"
        ),
    )
    .await
}

#[tokio::test]
#[serial]
async fn a_batch_whose_alarm_backfill_cannot_be_queued_stores_nothing() {
    let fx = setup().await;

    refuse_enqueue(&fx.db, "alarm_backfill").await;
    let (status, body) = batch(&fx).await;
    restore_enqueue(&fx.db).await;

    assert_ne!(status, 200, "the batch reports the failure: {body}");
    assert_eq!(stored_at(&fx.db, AT).await, 0, "and stores nothing");

    let (status, body) = batch(&fx).await;
    assert_eq!(status, 200, "a retry stores the reading: {body}");
    assert_eq!(stored_at(&fx.db, AT).await, 1);
    assert_eq!(queued(&fx.db, "alarm_backfill").await, 1);

    crate::common::cleanup_test_db(&fx.db).await;
}

/// A stored instrument pin on one reading, recorded as its own set. Nothing records a pin (Q117),
/// so the one a rollback inverts is a stored row. Returns the decision and its set.
async fn stored_pin(db: &DatabaseConnection) -> (Uuid, Uuid) {
    let stream = continuous_reading(db).await;
    let sensor = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO sensors (id, name, is_active, source_system, source_key) \
             VALUES ('{sensor}', 'Pinned', true, 'pinsrc', 'pinsrc:1')"
        ),
    )
    .await;
    let set = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO reading_decision_sets (id, kind, selection, actor) \
             VALUES ('{set}', 'instrument_pin', jsonb_build_object('stream_id', '{stream}'), 'test')"
        ),
    )
    .await;
    let decision = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO reading_decisions \
                 (id, stream_id, time, replicate_index, kind, old, new, actor, origin, set_id) \
             VALUES ('{decision}', '{stream}', '{AT}', 0, 'instrument_pin', \
                     jsonb_build_object('sensor_id', NULL), \
                     jsonb_build_object('sensor_id', '{sensor}'), 'test', 'manual', '{set}')"
        ),
    )
    .await;
    (decision, set)
}

async fn rolled_back(db: &DatabaseConnection, decision: Uuid) -> i64 {
    count(
        db,
        &format!(
            "SELECT COUNT(*)::bigint FROM reading_decisions \
             WHERE id = '{decision}' AND rolled_back_by IS NOT NULL"
        ),
    )
    .await
}

#[tokio::test]
#[serial]
async fn a_pin_rollback_whose_reprocess_cannot_be_queued_restores_nothing() {
    let fx = setup().await;
    let (decision, _) = stored_pin(&fx.db).await;
    let uri = format!("/api/readings/edits/{decision}/rollback");

    refuse_enqueue(&fx.db, "attribution_pin").await;
    let (status, body) =
        crate::common::post_json_with_token(&fx.app, &uri, &json!({}), &fx.token).await;
    restore_enqueue(&fx.db).await;

    assert_ne!(status, 200, "the rollback reports the failure: {body}");
    assert_eq!(
        rolled_back(&fx.db, decision).await,
        0,
        "and inverts nothing"
    );

    let (status, body) =
        crate::common::post_json_with_token(&fx.app, &uri, &json!({}), &fx.token).await;
    assert_eq!(status, 200, "a retry inverts the pin: {body}");
    assert_eq!(rolled_back(&fx.db, decision).await, 1);
    assert_eq!(queued(&fx.db, "attribution_pin").await, 1);

    crate::common::cleanup_test_db(&fx.db).await;
}

#[tokio::test]
#[serial]
async fn a_pin_set_rollback_whose_reprocess_cannot_be_queued_restores_nothing() {
    let fx = setup().await;
    let (decision, set) = stored_pin(&fx.db).await;
    let uri = format!("/api/readings/edits/sets/{set}/rollback");

    refuse_enqueue(&fx.db, "attribution_pin").await;
    let (status, body) =
        crate::common::post_json_with_token(&fx.app, &uri, &json!({}), &fx.token).await;
    restore_enqueue(&fx.db).await;

    assert_ne!(status, 200, "the set rollback reports the failure: {body}");
    assert_eq!(
        rolled_back(&fx.db, decision).await,
        0,
        "and inverts nothing"
    );

    let (status, body) =
        crate::common::post_json_with_token(&fx.app, &uri, &json!({}), &fx.token).await;
    assert_eq!(status, 200, "a retry inverts the set: {body}");
    assert_eq!(rolled_back(&fx.db, decision).await, 1);
    assert_eq!(queued(&fx.db, "attribution_pin").await, 1);

    crate::common::cleanup_test_db(&fx.db).await;
}

/// Wait until the import job has made its first attempt, whatever it ended in.
async fn first_attempt(db: &DatabaseConnection) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let done = count(
            db,
            "SELECT COUNT(*)::bigint FROM reprocessing_jobs \
             WHERE trigger_type = 'csv_import' \
               AND (status IN ('completed', 'failed') OR retry_count > 0)",
        )
        .await;
        if done > 0 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the import job never ran: {}",
            crate::common::e2e::jobs_summary(db).await
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

#[tokio::test]
#[serial]
async fn an_import_whose_alarm_backfill_cannot_be_queued_imports_nothing() {
    let fx = crate::common::seeded_app().await;
    const AT_CSV: &str = "2025-06-15 10:04:00";

    refuse_enqueue(&fx.db, "alarm_backfill").await;
    let (status, body) = crate::common::post_screened_import(
        &fx.app,
        &json!({
            "site": SITE1_ID,
            "csv": format!("DateTime,DO_Temperature\n{AT_CSV},12.5\n"),
        }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "the import is staged: {body}");
    first_attempt(&fx.db).await;
    restore_enqueue(&fx.db).await;

    assert_eq!(
        stored_at(&fx.db, AT).await,
        0,
        "an import that could not queue its episodes rebuild stores nothing: {}",
        crate::common::e2e::jobs_summary(&fx.db).await
    );

    crate::common::cleanup_test_db(&fx.db).await;
}

/// An active derived slot at the site, so a write there queues its derived job.
async fn derived_slot(db: &DatabaseConnection) {
    let parameter = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
             VALUES ('{parameter}', 'FollowUpDerived', 'Follow-up derived', 'x', 'measurement')"
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO site_parameters \
                 (id, site_id, parameter_id, name, sensor_type, is_active, entry_mode) \
             VALUES ('{}', '{SITE1_ID}', '{parameter}', 'FollowUpDerived', 'FollowUpDerived', \
                     true, 'tool')",
            Uuid::new_v4()
        ),
    )
    .await;
}

#[tokio::test]
#[serial]
async fn a_batch_whose_derived_job_cannot_be_queued_stores_nothing() {
    let fx = setup().await;
    derived_slot(&fx.db).await;

    refuse_enqueue(&fx.db, "batch_derived").await;
    let (status, body) = batch(&fx).await;
    restore_enqueue(&fx.db).await;

    assert_ne!(status, 200, "the batch reports the failure: {body}");
    assert_eq!(stored_at(&fx.db, AT).await, 0, "and stores nothing");

    let (status, body) = batch(&fx).await;
    assert_eq!(status, 200, "a retry stores the reading: {body}");
    assert_eq!(stored_at(&fx.db, AT).await, 1);
    assert_eq!(queued(&fx.db, "batch_derived").await, 1);

    crate::common::cleanup_test_db(&fx.db).await;
}

async fn ingest(fx: &Fixture, stream: Uuid, at: &str) -> (u16, String) {
    crate::common::post_json_with_token(
        &fx.app,
        "/api/ingest",
        &json!({
            "stream_id": stream,
            "readings": [{ "time": at, "raw_value": 12.5 }],
        }),
        &fx.token,
    )
    .await
}

#[tokio::test]
#[serial]
async fn an_ingest_whose_derived_job_cannot_be_queued_stores_nothing() {
    const LATER: &str = "2025-06-15T10:14:00Z";
    let fx = setup().await;
    let stream = continuous_reading(&fx.db).await;
    derived_slot(&fx.db).await;

    refuse_enqueue(&fx.db, "ingest_derived").await;
    let (status, body) = ingest(&fx, stream, LATER).await;
    restore_enqueue(&fx.db).await;

    assert_ne!(status, 200, "the ingest reports the failure: {body}");
    assert_eq!(stored_at(&fx.db, LATER).await, 0, "and stores nothing");

    let (status, body) = ingest(&fx, stream, LATER).await;
    assert_eq!(status, 200, "a retry stores the reading: {body}");
    assert_eq!(stored_at(&fx.db, LATER).await, 1);
    assert_eq!(queued(&fx.db, "ingest_derived").await, 1);

    crate::common::cleanup_test_db(&fx.db).await;
}
