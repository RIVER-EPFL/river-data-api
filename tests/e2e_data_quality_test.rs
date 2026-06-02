//! End-to-end data-quality configuration: set an alarm threshold, annotate a time range, and add a
//! station note (US-3.1, US-4.1, US-4.3). Plus an aspirational, currently-blocked stateful
//! alarm acknowledge/auto-resolve flow (US-3.2).
//!
//! Run: cargo test --test e2e_data_quality_test -- --test-threads=1

mod common;

use common::e2e;
use common::sensor_lifecycle as sl;
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

/// Aspirational (US-3.2): acknowledging an alarm and having it auto-resolve when readings return to
/// range. BLOCKED — alarms are computed on-the-fly (stateless); there is no `alarm_events` table,
/// no `POST /api/alarms/{id}/acknowledge`, and no resolve/re-raise state machine (see CLAUDE.md
/// "Deferred"). Encoded so the workflow exists once that feature is built.
#[tokio::test]
#[serial]
#[ignore = "BLOCKED: alarms are stateless — no alarm_events table / acknowledge endpoint (CLAUDE.md Deferred)"]
async fn alarm_acknowledge_and_autoresolve() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());

    // Intended: ingest an out-of-range reading → it appears in /api/alarms/active → acknowledge it
    // (POST /api/alarms/{id}/acknowledge) → it shows acknowledged → a later in-range reading
    // auto-resolves it. No acknowledge endpoint exists yet.
    let (status, _ack) = common::post_json_with_token(
        &app,
        "/api/alarms/acknowledge",
        &serde_json::json!({ "site_id": common::SITE1_ID, "parameter_id": common::GLOBAL_PARAM_TURB_ID }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "acknowledge endpoint should exist and succeed (got {status})");
}
