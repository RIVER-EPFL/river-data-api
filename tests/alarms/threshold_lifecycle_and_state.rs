//! Alarm threshold-override lifecycle + sweeper behaviours surfaced by the UI's Reset/Disable
//! actions and the re-raise / unacknowledge flows. Driven through the persisted `alarm_events`
//! + sweeper for determinism (the periodic task is never spawned under `build_test_app`).
//!
//! Run: cargo test --test alarms -- --test-threads=1


use river_db::routes::private::alarms;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;
use uuid::Uuid;

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
    db.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{stream_id}', '{site}', '{param}', '{time}', {value}, 0) ON CONFLICT DO NOTHING",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    ))
    .await
    .unwrap();
}

async fn open_turb_event_count(db: &sea_orm::DatabaseConnection) -> i64 {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT COUNT(*) AS c FROM alarm_events \
             WHERE site_id='{}' AND parameter_id='{}' AND resolved_at IS NULL",
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

/// Deleting a site-specific override (the UI's "Reset to defaults") re-enables alarms via the
/// fallback chain: a value that the wide override let pass now breaches once the override is gone.
#[tokio::test]
#[serial]
async fn reset_to_defaults_re_enables_alarms() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await; // global Turbidity threshold: alarm > 500
    let stream = turb_stream(&db).await;

    // A wide site-specific override that lets everything pass.
    db.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "INSERT INTO alarm_thresholds (id, parameter_id, site_id, warning_max, alarm_max) \
             VALUES (gen_random_uuid(), '{}', '{}', 99999, 99999)",
            crate::common::GLOBAL_PARAM_TURB_ID,
            crate::common::SITE1_ID,
        ),
    ))
    .await
    .unwrap();

    inject(&db, stream, "2025-02-01T00:00:00Z", 600.0).await;
    alarms::sweeper::evaluate_alarm_events(&db).await.unwrap();
    assert_eq!(
        open_turb_event_count(&db).await,
        0,
        "the wide override suppresses the breach"
    );

    // Reset to defaults = delete the site-specific row → falls back to the global threshold.
    db.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "DELETE FROM alarm_thresholds WHERE parameter_id='{}' AND site_id='{}'",
            crate::common::GLOBAL_PARAM_TURB_ID,
            crate::common::SITE1_ID,
        ),
    ))
    .await
    .unwrap();

    let stats = alarms::sweeper::evaluate_alarm_events(&db).await.unwrap();
    assert!(stats.opened >= 1, "fallback re-enables the alarm: {stats:?}");
    assert_eq!(open_turb_event_count(&db).await, 1);
}

/// "Disable alarms" writes an all-NULL override at priority 1 that blocks the fallback: it both
/// suppresses new breaches AND auto-resolves an already-open event on the next sweep.
#[tokio::test]
#[serial]
async fn disable_alarms_null_row_suppresses_and_resolves() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let stream = turb_stream(&db).await;

    inject(&db, stream, "2025-02-01T00:00:00Z", 600.0).await;
    let stats = alarms::sweeper::evaluate_alarm_events(&db).await.unwrap();
    assert!(stats.opened >= 1, "breach opens an event: {stats:?}");
    assert_eq!(open_turb_event_count(&db).await, 1);

    // Disable alarms: an all-NULL site row at priority 1.
    db.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "INSERT INTO alarm_thresholds (id, parameter_id, site_id, warning_min, warning_max, alarm_min, alarm_max) \
             VALUES (gen_random_uuid(), '{}', '{}', NULL, NULL, NULL, NULL)",
            crate::common::GLOBAL_PARAM_TURB_ID,
            crate::common::SITE1_ID,
        ),
    ))
    .await
    .unwrap();

    let stats = alarms::sweeper::evaluate_alarm_events(&db).await.unwrap();
    assert!(stats.resolved >= 1, "disabling resolves the open event: {stats:?}");
    assert_eq!(
        open_turb_event_count(&db).await,
        0,
        "no open events once alarms are disabled"
    );
}

/// After an event resolves, a fresh breach of the same slot opens a brand-new event (history of the
/// first is preserved) rather than re-opening the resolved one.
#[tokio::test]
#[serial]
async fn rebreach_opens_fresh_event() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let stream = turb_stream(&db).await;

    let event_ids = || async {
        db.query_all(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT id FROM alarm_events WHERE site_id='{}' AND parameter_id='{}' ORDER BY started_at",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TURB_ID,
            ),
        ))
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.try_get::<Uuid>("", "id").unwrap())
        .collect::<Vec<_>>()
    };

    inject(&db, stream, "2025-02-01T00:00:00Z", 600.0).await;
    alarms::sweeper::evaluate_alarm_events(&db).await.unwrap();
    let first = event_ids().await;
    assert_eq!(first.len(), 1, "first breach opens one event");

    inject(&db, stream, "2025-02-01T01:00:00Z", 50.0).await;
    alarms::sweeper::evaluate_alarm_events(&db).await.unwrap();

    inject(&db, stream, "2025-02-01T02:00:00Z", 700.0).await;
    alarms::sweeper::evaluate_alarm_events(&db).await.unwrap();
    let after = event_ids().await;

    assert_eq!(after.len(), 2, "re-breach opens a second event, history preserved: {after:?}");
    assert_ne!(after[0], after[1], "the re-raise is a new event id");
    assert_eq!(after[0], first[0], "the original event is still present");
}

/// Acknowledge then unacknowledge clears the acknowledgement state on the still-open event.
#[tokio::test]
#[serial]
async fn unacknowledge_clears_acknowledgement() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let stream = turb_stream(&db).await;

    inject(&db, stream, "2025-02-01T00:00:00Z", 600.0).await;
    alarms::sweeper::evaluate_alarm_events(&db).await.unwrap();

    let find_turb = |active: &serde_json::Value| -> Option<serde_json::Value> {
        active["alarms"]
            .as_array()?
            .iter()
            .find(|a| a["parameter_id"].as_str() == Some(crate::common::GLOBAL_PARAM_TURB_ID))
            .cloned()
    };

    let (_s, active) = crate::common::get_json_with_token(&app, "/api/alarms/active", &token).await;
    let event_id = find_turb(&active).unwrap()["event_id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, _b) = crate::common::post_json_with_token(
        &app,
        &format!("/api/alarms/{event_id}/acknowledge"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "acknowledge ({status})");
    let (_s, active) = crate::common::get_json_with_token(&app, "/api/alarms/active", &token).await;
    assert_eq!(find_turb(&active).unwrap()["acknowledged"], serde_json::json!(true));

    let (status, _b) = crate::common::delete_with_token(
        &app,
        &format!("/api/alarms/{event_id}/acknowledge"),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "unacknowledge ({status})");
    let (_s, active) = crate::common::get_json_with_token(&app, "/api/alarms/active", &token).await;
    assert_eq!(
        find_turb(&active).unwrap()["acknowledged"],
        serde_json::json!(false),
        "unacknowledge clears the flag"
    );
}
