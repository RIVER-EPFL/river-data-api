//! End-to-end data-curation + quality-control workflow: merge one site_parameter into another
//! (WS4), then flag/unflag a reading as an outlier (US-4.2) and query the alarm feed (US-2.2/3.2).
//!
//! Run: cargo test --test e2e -- --test-threads=1


use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;

#[tokio::test]
#[serial]
async fn merge_site_parameters_then_flag_and_query_alarms() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let site1 = crate::common::SITE1_ID;

    // --- WS4: merge one site_parameter into another (reassigns readings/streams, deletes source) ---
    let (status, merge) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/merge_site_parameters",
        &serde_json::json!({
            "source_site_parameter_id": crate::common::PARAM_S1_TEMP_ID,
            "target_site_parameter_id": crate::common::PARAM_S1_DEPTH_ID,
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "merge ({status}): {merge}");
    assert_eq!(merge["source_deleted"], true, "source site_parameter deleted: {merge}");
    assert!(merge["merged_readings"].as_i64().unwrap() > 0, "some readings reassigned: {merge}");

    // Source site_parameter is gone; its readings now carry the target's parameter_id.
    let (status, _gone) =
        crate::common::get_with_token(&app, &format!("/api/site_parameters/{}", crate::common::PARAM_S1_TEMP_ID), &token).await;
    assert_eq!(status, 404, "merged-away source site_parameter should be gone");
    let remaining_temp: i64 = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COUNT(*) AS c FROM readings WHERE site_id='{site1}' AND parameter_id='{}'",
                crate::common::GLOBAL_PARAM_TEMP_ID
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "c")
        .unwrap();
    assert_eq!(remaining_temp, 0, "no readings should remain under the source parameter after merge");

    // --- US-4.2: flag a reading as an outlier, then unflag it ---
    let flag_time = "2025-01-15T00:00:00Z";
    let key = serde_json::json!({ "site_id": site1, "parameter_id": crate::common::GLOBAL_PARAM_COND_ID, "time": flag_time });

    let (status, body) = crate::common::patch_json_with_token(
        &app,
        "/api/readings/flag",
        &serde_json::json!({ "readings": [key], "reason": "e2e outlier" }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "flag ({status}): {body}");

    let flagged: bool = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT is_flagged FROM readings WHERE site_id='{site1}' AND parameter_id='{}' AND time='{flag_time}'",
                crate::common::GLOBAL_PARAM_COND_ID
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "is_flagged")
        .unwrap();
    assert!(flagged, "reading should be flagged");

    let (status, body) = crate::common::patch_json_with_token(
        &app,
        "/api/readings/unflag",
        &serde_json::json!({ "readings": [key] }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "unflag ({status}): {body}");
    let still_flagged: bool = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT is_flagged FROM readings WHERE site_id='{site1}' AND parameter_id='{}' AND time='{flag_time}'",
                crate::common::GLOBAL_PARAM_COND_ID
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "is_flagged")
        .unwrap();
    assert!(!still_flagged, "reading should be unflagged");

    // --- US-2.2 / US-3.2: the active-alarm feed and summary respond with the expected shape ---
    let (status, active) = crate::common::get_json_with_token(&app, "/api/alarms/active", &token).await;
    assert_eq!(status, 200, "alarms/active ({status}): {active}");
    assert!(active["alarms"].is_array(), "alarms/active has an alarms array: {active}");
    assert!(active["total"].is_number(), "alarms/active has a total: {active}");

    let (status, summary) = crate::common::get_json_with_token(&app, "/api/alarms/summary", &token).await;
    assert_eq!(status, 200, "alarms/summary ({status}): {summary}");
}
