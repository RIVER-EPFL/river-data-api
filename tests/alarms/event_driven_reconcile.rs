//! Event-driven alarm reconciliation: every write or config change that can alter breach state
//! updates persisted `alarm_events` immediately, without the periodic backstop sweep. Assertions
//! deliberately never call `evaluate_alarm_events` except where the backstop itself is the
//! behaviour under test (`build_test_app` never spawns the sweeper task, so any state observed
//! here came from the event-driven triggers).
//!
//! Run: cargo test --test alarms -- --test-threads=1

use river_db::routes::private::alarms;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;
use uuid::Uuid;

const BREACH: f64 = 600.0; // turbidity global threshold: warning > 100, alarm > 500
const IN_RANGE: f64 = 50.0;

async fn turb_stream(db: &sea_orm::DatabaseConnection) -> Uuid {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT stream_id FROM readings WHERE site_id='{}' AND parameter_id='{}' LIMIT 1",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    ))
    .await
    .unwrap()
    .expect("a seeded turbidity stream")
    .try_get("", "stream_id")
    .unwrap()
}

async fn inject(db: &sea_orm::DatabaseConnection, stream_id: Uuid, time: &str, value: f64) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{stream_id}', '{site}', '{param}', '{time}', {value}, 0) ON CONFLICT DO NOTHING",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    )
    .await;
}

async fn turb_event_count(db: &sea_orm::DatabaseConnection, only_open: bool) -> i64 {
    let resolved_filter = if only_open { "AND resolved_at IS NULL" } else { "" };
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT COUNT(*) AS c FROM alarm_events \
             WHERE site_id='{}' AND parameter_id='{}' {resolved_filter}",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "c")
    .unwrap()
}

async fn open_turb_event_count(db: &sea_orm::DatabaseConnection) -> i64 {
    turb_event_count(db, true).await
}

/// `POST /readings/batch` reconciles the written slots before responding: a breach opens an event
/// and a return-to-range resolves it, with no sweep in between.
#[tokio::test]
#[serial]
async fn batch_ingest_opens_and_resolves_immediately() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let reading = |time: &str, value: f64| {
        serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_TURB_ID,
            "time": time,
            "raw_value": value,
        })
    };

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &serde_json::json!({ "readings": [reading("2025-02-01T00:00:00Z", BREACH)] }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "batch insert: {body}");
    assert_eq!(open_turb_event_count(&db).await, 1, "breach opens without a sweep");

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &serde_json::json!({ "readings": [reading("2025-02-01T01:00:00Z", IN_RANGE)] }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "batch insert: {body}");
    assert_eq!(open_turb_event_count(&db).await, 0, "return-to-range resolves without a sweep");
}

/// `POST /ingest` (the per-stream sync path) reconciles the slot AND reconstructs historical
/// episodes over the ingested window: a back-dated breach followed by an in-range reading lands as
/// an already-resolved event.
#[tokio::test]
#[serial]
async fn single_ingest_reconciles_and_reconstructs_episodes() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let stream = turb_stream(&db).await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/ingest",
        &serde_json::json!({
            "stream_id": stream,
            "readings": [
                { "time": "2025-02-01T00:00:00Z", "raw_value": BREACH },
                { "time": "2025-02-01T01:00:00Z", "raw_value": IN_RANGE },
            ],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "ingest: {body}");

    assert_eq!(open_turb_event_count(&db).await, 0, "latest reading is in range");
    assert_eq!(
        turb_event_count(&db, false).await,
        1,
        "the back-dated breach was reconstructed as a resolved episode"
    );
}

/// `POST /grab_samples` reconciles the sampled slots and reconstructs episodes the same way.
#[tokio::test]
#[serial]
async fn grab_samples_reconcile_and_reconstruct_episodes() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let grab = |time: &str, value: f64| {
        serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "readings": [{
                "parameter_id": crate::common::GLOBAL_PARAM_TURB_ID,
                "value": value,
                "time": time,
            }],
        })
    };

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &grab("2025-02-01T00:00:00Z", BREACH),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab insert: {body}");
    assert_eq!(open_turb_event_count(&db).await, 1, "grab breach opens without a sweep");

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &grab("2025-02-01T01:00:00Z", IN_RANGE),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab insert: {body}");
    assert_eq!(open_turb_event_count(&db).await, 0, "in-range grab resolves without a sweep");
}

/// A scoped reconcile never touches slots outside its scope: a stale open event on one slot
/// survives a reconcile targeted at another, and only the unscoped backstop clears it.
#[tokio::test]
#[serial]
async fn scoped_reconcile_leaves_other_slots_untouched() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let stream = turb_stream(&db).await;

    inject(&db, stream, "2025-02-01T00:00:00Z", BREACH).await;
    alarms::sweeper::evaluate_alarm_events(&db).await.unwrap();
    assert_eq!(open_turb_event_count(&db).await, 1);

    // Make the open event stale: with the breaching reading gone, any reconcile that considers
    // this slot would resolve the event.
    crate::common::exec(
        &db,
        &format!(
            "DELETE FROM readings WHERE site_id='{}' AND parameter_id='{}' AND raw_value={BREACH}",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    )
    .await;

    let site: Uuid = crate::common::SITE1_ID.parse().unwrap();
    let temp: Uuid = crate::common::GLOBAL_PARAM_TEMP_ID.parse().unwrap();
    alarms::sweeper::reconcile_open_alarms(&db, &[(site, temp)]).await.unwrap();
    assert_eq!(
        open_turb_event_count(&db).await,
        1,
        "a reconcile scoped to the temperature slot must not resolve the turbidity event"
    );

    alarms::sweeper::evaluate_alarm_events(&db).await.unwrap();
    assert_eq!(open_turb_event_count(&db).await, 0, "the unscoped backstop resolves it");
}

/// Threshold CRUD re-checks breach state instantly — create, update, and delete each flip the
/// persisted event with no new reading and no sweep.
#[tokio::test]
#[serial]
async fn threshold_crud_reconciles_without_new_reading() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let stream = turb_stream(&db).await;

    inject(&db, stream, "2025-02-01T00:00:00Z", BREACH).await;
    assert_eq!(open_turb_event_count(&db).await, 0, "no trigger has fired yet");

    // Create a wide site override: the breach is suppressed, so nothing opens.
    let (status, created) = crate::common::post_json_parse_with_token(
        &app,
        "/api/alarm_thresholds",
        &serde_json::json!({
            "parameter_id": crate::common::GLOBAL_PARAM_TURB_ID,
            "site_id": crate::common::SITE1_ID,
            "warning_max": 99999.0,
            "alarm_max": 99999.0,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 201, "threshold create: {created}");
    let override_id = created["id"].as_str().unwrap().to_string();
    assert_eq!(open_turb_event_count(&db).await, 0, "wide override suppresses the breach");

    // Narrow it: the standing reading now breaches, and the update hook opens the event.
    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/alarm_thresholds/{override_id}"),
        &serde_json::json!({ "warning_max": 5.0, "alarm_max": 10.0 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "threshold update: {body}");
    assert_eq!(open_turb_event_count(&db).await, 1, "narrowing opens without a sweep");

    // Widen it back: the update hook resolves the event.
    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/alarm_thresholds/{override_id}"),
        &serde_json::json!({ "warning_max": 99999.0, "alarm_max": 99999.0 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "threshold update: {body}");
    assert_eq!(open_turb_event_count(&db).await, 0, "widening resolves without a sweep");

    // Delete the override: fallback to the global threshold (alarm > 500) re-opens.
    let (status, body) =
        crate::common::delete_with_token(&app, &format!("/api/alarm_thresholds/{override_id}"), &token)
            .await;
    assert!((200..300).contains(&status), "threshold delete ({status}): {body}");
    assert_eq!(open_turb_event_count(&db).await, 1, "delete falls back and opens without a sweep");
}

/// Editing a parameter's `default_*` columns re-checks breach state instantly when no
/// `alarm_thresholds` row exists (the default tier is resolved live).
#[tokio::test]
#[serial]
async fn parameter_default_change_reconciles_without_new_reading() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let stream = turb_stream(&db).await;

    crate::common::exec(&db, "DELETE FROM alarm_thresholds").await;
    crate::common::exec(&db, "DELETE FROM alarm_events").await;
    inject(&db, stream, "2025-02-01T00:00:00Z", BREACH).await;
    assert_eq!(open_turb_event_count(&db).await, 0, "no thresholds, no trigger fired");

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/parameters/{}", crate::common::GLOBAL_PARAM_TURB_ID),
        &serde_json::json!({ "default_warning_max": 100.0, "default_alarm_max": 500.0 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "parameter update: {body}");
    assert_eq!(open_turb_event_count(&db).await, 1, "tight defaults open without a sweep");

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/parameters/{}", crate::common::GLOBAL_PARAM_TURB_ID),
        &serde_json::json!({ "default_warning_max": 99999.0, "default_alarm_max": 99999.0 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "parameter update: {body}");
    assert_eq!(open_turb_event_count(&db).await, 0, "wide defaults resolve without a sweep");
}

/// Deactivating a site_parameter removes its slot from evaluation and resolves its open event
/// instantly (breach evaluation only considers active slots).
#[tokio::test]
#[serial]
async fn site_parameter_deactivation_resolves_open_event() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let stream = turb_stream(&db).await;

    inject(&db, stream, "2025-02-01T00:00:00Z", BREACH).await;
    alarms::sweeper::evaluate_alarm_events(&db).await.unwrap();
    assert_eq!(open_turb_event_count(&db).await, 1);

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/site_parameters/{}", crate::common::PARAM_S1_TURB_ID),
        &serde_json::json!({ "is_active": false }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "site_parameter update: {body}");
    assert_eq!(open_turb_event_count(&db).await, 0, "deactivation resolves without a sweep");
}

/// Among same-timestamp replicates, only replicate 0 decides breach state — a breaching
/// replicate 1 must not open an alarm when replicate 0 is in range.
#[tokio::test]
#[serial]
async fn replicate_zero_decides_breach_state() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let stream = turb_stream(&db).await;

    for (replicate, value) in [(0, IN_RANGE), (1, BREACH)] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
                 VALUES ('{stream}', '{site}', '{param}', '2025-02-01T00:00:00Z', {value}, {replicate}) \
                 ON CONFLICT DO NOTHING",
                site = crate::common::SITE1_ID,
                param = crate::common::GLOBAL_PARAM_TURB_ID,
            ),
        )
        .await;
    }

    alarms::sweeper::evaluate_alarm_events(&db).await.unwrap();
    assert_eq!(
        open_turb_event_count(&db).await,
        0,
        "a breaching non-zero replicate must not drive the alarm"
    );
}

/// `POST /actions/reconcile_alarms` runs a full reconcile on demand and reports what changed.
#[tokio::test]
#[serial]
async fn reconcile_alarms_action_runs_full_reconcile() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let stream = turb_stream(&db).await;

    inject(&db, stream, "2025-02-01T00:00:00Z", BREACH).await;
    assert_eq!(open_turb_event_count(&db).await, 0, "no trigger fired for the raw insert");

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/reconcile_alarms",
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "reconcile action: {body}");
    assert!(
        body["opened"].as_i64().unwrap() >= 1,
        "the action reports the opened event: {body}"
    );
    assert_eq!(open_turb_event_count(&db).await, 1);
}
