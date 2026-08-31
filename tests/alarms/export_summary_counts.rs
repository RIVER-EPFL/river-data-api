//! Scenario: a site whose history breaches thresholds but whose `alarm_events` table is empty (the
//! sweeper only records episodes from when it first saw a slot, while an export covers all of
//! history).
//!
//! Expected behaviour: the export summary counts what `/sites/{id}/alarms` would return, so the
//! number shown beside the alarms option describes the file that option downloads.
//!
//! Run: cargo test --test alarms -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;
use uuid::Uuid;

#[tokio::test]
#[serial]
async fn breaching_readings_count_without_an_alarm_event_row() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let site = crate::common::SITE1_ID;
    let turb = crate::common::GLOBAL_PARAM_TURB_ID;

    crate::common::exec(&db, "DELETE FROM alarm_thresholds").await;
    crate::common::exec(&db, "DELETE FROM alarm_events").await;
    crate::common::exec(
        &db,
        &format!("UPDATE parameters SET default_alarm_max = 500 WHERE id = '{turb}'"),
    )
    .await;

    let stream_id: Uuid = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT stream_id FROM readings WHERE site_id='{site}' AND parameter_id='{turb}' LIMIT 1"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "stream_id")
        .unwrap();
    for (minute, value) in [("00", 9999), ("10", 8888), ("20", 12)] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
                 VALUES ('{stream_id}', '{site}', '{turb}', '2025-02-01T00:{minute}:00Z', {value}, 0) \
                 ON CONFLICT DO NOTHING"
            ),
        )
        .await;
    }

    let range = "start=2025-02-01T00:00:00Z&end=2025-02-01T01:00:00Z";
    let (status, summary) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site}/export/summary?{range}"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "summary ({status}): {summary}");
    assert_eq!(
        summary["alarm_readings"], 2,
        "the two breaching readings, not the zero alarm episodes: {summary}"
    );

    let (status, alarms) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site}/alarms?{range}&parameter_ids={turb}"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "alarms ({status}): {alarms}");
    assert_eq!(
        alarms["times"].as_array().map(Vec::len),
        Some(2),
        "the export the count gates carries exactly those rows: {alarms}"
    );
}
