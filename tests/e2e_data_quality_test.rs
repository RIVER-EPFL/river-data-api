//! End-to-end data-quality configuration: set an alarm threshold, annotate a time range, and add a
//! station note (US-3.1, US-4.1, US-4.3). Plus the stateful alarm acknowledge / auto-resolve flow
//! (US-3.2) driven through the persisted `alarm_events` + sweeper.
//!
//! Run: cargo test --test e2e_data_quality_test -- --test-threads=1

mod common;

use common::e2e;
use common::sensor_lifecycle as sl;
use river_db::routes::private::alarms;
use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;

#[tokio::test]
#[serial]
async fn configure_threshold_annotate_and_note() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await; // project, sites, params, site_params (no thresholds/readings)
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());

    let site1 = common::SITE1_ID;
    let turb = common::GLOBAL_PARAM_TURB_ID;

    // US-3.1: configure an alarm threshold for a parameter at this site.
    let (status, thr) = common::post_json_parse_with_token(
        &app,
        "/api/alarm_thresholds",
        &serde_json::json!({
            "parameter_id": turb, "site_id": site1,
            "warning_min": 0.0, "warning_max": 100.0, "alarm_min": -1.0, "alarm_max": 500.0,
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create threshold ({status}): {thr}");
    let thr_id = e2e::id_of(&thr);
    let (status, got) = common::get_json_with_token(&app, &format!("/api/alarm_thresholds/{thr_id}"), &token).await;
    assert_eq!(status, 200, "get threshold");
    assert_eq!(got["alarm_max"].as_f64(), Some(500.0), "threshold persisted: {got}");

    // US-4.1: annotate a time range on the parameter, then read it back via the site annotations.
    let (status, ann) = common::post_json_with_token(
        &app,
        "/api/annotations",
        &serde_json::json!({
            "site_id": site1, "parameter_id": turb,
            "start_time": "2025-01-15T00:00:00Z", "end_time": "2025-01-15T06:00:00Z",
            "text": "sensor fouling suspected", "category": "quality_issue",
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create annotation ({status}): {ann}");
    let (status, anns) = common::get_json_with_token(&app, &format!("/api/sites/{site1}/annotations"), &token).await;
    assert_eq!(status, 200, "list annotations ({status}): {anns}");
    let list = anns.as_array().cloned().unwrap_or_else(|| anns["annotations"].as_array().cloned().unwrap_or_default());
    assert!(
        list.iter().any(|a| a["text"] == "sensor fouling suspected"),
        "annotation should appear for the site: {anns}"
    );

    // US-4.3: add a station note and confirm it lists.
    let (status, note) = common::post_json_parse_with_token(
        &app,
        "/api/notes",
        &serde_json::json!({ "site_id": site1, "text": "Visited station; cleaned optics." }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create note ({status}): {note}");
    let note_id = e2e::id_of(&note);
    let (status, notes) = common::get_json_with_token(&app, "/api/notes?page_size=100", &token).await;
    assert_eq!(status, 200, "list notes ({status})");
    let notes_list = notes.as_array().cloned().unwrap_or_else(|| notes["data"].as_array().cloned().unwrap_or_default());
    assert!(notes_list.iter().any(|n| n["id"].as_str() == Some(note_id.as_str())), "note should list");
}

/// US-3.2: an out-of-range reading opens a persisted alarm event (sweeper), the event can be
/// acknowledged while still firing, and a later in-range reading auto-resolves it. The sweeper never
/// runs under `build_test_app`, so the test drives `evaluate_alarm_events` directly for determinism.
#[tokio::test]
#[serial]
async fn alarm_acknowledge_and_autoresolve() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await; // thresholds + in-range readings for SITE1/Turbidity (OK band 0..100)
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());

    let site1 = common::SITE1_ID;
    let turb = common::GLOBAL_PARAM_TURB_ID;

    // Reuse a seeded stream for SITE1/Turbidity to inject readings at later-than-seed timestamps so
    // they become the latest reading the sweeper evaluates.
    let stream_id: uuid::Uuid = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT stream_id FROM readings WHERE site_id='{site1}' AND parameter_id='{turb}' LIMIT 1"),
        ))
        .await
        .unwrap()
        .expect("a seeded turbidity stream")
        .try_get("", "stream_id")
        .unwrap();

    let inject = |time: &str, value: f64| {
        let sql = format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{stream_id}', '{site1}', '{turb}', '{time}', {value}, 0) ON CONFLICT DO NOTHING"
        );
        db.execute(Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql))
    };

    // Out-of-range turbidity (> alarm_max 500) at the latest time → a breach.
    inject("2025-02-01T00:00:00Z", 9999.0).await.unwrap();

    let stats = alarms::sweeper::evaluate_alarm_events(&db).await.unwrap();
    assert!(stats.opened >= 1, "the breach should open an alarm event: {stats:?}");

    // It appears in /alarms/active with a stable event_id, severity 2, unacknowledged.
    let find_turb = |active: &serde_json::Value| -> Option<serde_json::Value> {
        active["alarms"].as_array()?.iter()
            .find(|a| a["site_id"].as_str() == Some(site1) && a["parameter_id"].as_str() == Some(turb))
            .cloned()
    };
    let (status, active) = common::get_json_with_token(&app, "/api/alarms/active", &token).await;
    assert_eq!(status, 200, "alarms/active ({status}): {active}");
    let alarm = find_turb(&active).expect("turbidity breach in active feed");
    assert_eq!(alarm["severity"].as_i64(), Some(2), "out-of-range turbidity is an alarm: {alarm}");
    assert_eq!(alarm["acknowledged"], serde_json::json!(false), "not acknowledged yet: {alarm}");
    let event_id = alarm["event_id"].as_str().expect("event_id present once swept").to_string();

    // Acknowledge it — still firing, now flagged acknowledged.
    let (status, ack) = common::post_json_with_token(
        &app,
        &format!("/api/alarms/{event_id}/acknowledge"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "acknowledge ({status}): {ack}");

    let (_status, active) = common::get_json_with_token(&app, "/api/alarms/active", &token).await;
    let alarm = find_turb(&active).expect("still firing after acknowledge");
    assert_eq!(alarm["acknowledged"], serde_json::json!(true), "now acknowledged: {alarm}");

    // Acknowledging again is idempotent (200), not a 409.
    let (status, _again) = common::post_json_with_token(
        &app,
        &format!("/api/alarms/{event_id}/acknowledge"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "re-acknowledge should be idempotent (got {status})");

    // A later in-range reading clears the breach; the next sweep auto-resolves the event.
    inject("2025-02-01T01:00:00Z", 50.0).await.unwrap();
    let stats = alarms::sweeper::evaluate_alarm_events(&db).await.unwrap();
    assert!(stats.resolved >= 1, "returning in-range should resolve the event: {stats:?}");

    let (_status, active) = common::get_json_with_token(&app, "/api/alarms/active", &token).await;
    assert!(find_turb(&active).is_none(), "resolved alarm should drop out of the active feed: {active}");

    let resolved: bool = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT resolved_at IS NOT NULL AS done FROM alarm_events WHERE id='{event_id}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "done")
        .unwrap();
    assert!(resolved, "the alarm event should be marked resolved");
}
