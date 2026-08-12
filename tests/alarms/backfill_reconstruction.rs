//! Comprehensive (historical) alarm-event reconstruction.
//!
//! Scenario: out-of-range readings that arrive via bulk ingestion (CSV import / batch) or are
//! backfilled later are NOT seen by the live 60s sweeper (it only inspects the latest reading), so
//! they must become breach episodes via the `alarm_backfill` job, automatically on ingest and on
//! demand via `POST /api/actions/rebuild_alarm_events`.
//!
//! Run: cargo test --test alarms -- --test-threads=1


use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;
use std::time::Duration;
use uuid::Uuid;

/// Poll `reprocessing_jobs` until the most recent `alarm_backfill` job reaches a terminal state.
/// Returns the terminal status. Panics on timeout.
async fn wait_for_alarm_backfill(db: &sea_orm::DatabaseConnection) -> String {
    for _ in 0..150 {
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT status FROM reprocessing_jobs WHERE trigger_type = 'alarm_backfill' \
                 ORDER BY created_at DESC LIMIT 1"
                    .to_string(),
            ))
            .await
            .unwrap();
        if let Some(r) = row {
            let status: String = r.try_get("", "status").unwrap();
            if status == "completed" || status == "failed" {
                return status;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("alarm_backfill job did not reach a terminal state within 30s");
}

/// (max_severity, resolved) for every alarm event at SITE1/Turbidity, ordered by start.
async fn turbidity_episodes(db: &sea_orm::DatabaseConnection) -> Vec<(i16, bool)> {
    db.query_all(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT max_severity, resolved_at IS NOT NULL AS resolved FROM alarm_events \
             WHERE site_id = '{site}' AND parameter_id = '{param}' ORDER BY started_at",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    ))
    .await
    .unwrap()
    .into_iter()
    .map(|r| {
        (
            r.try_get::<i16>("", "max_severity").unwrap(),
            r.try_get::<bool>("", "resolved").unwrap(),
        )
    })
    .collect()
}

/// Ingesting a CSV whose values cross the Turbidity warning/alarm bands automatically fires the
/// backfill job, which reconstructs the breach episodes (one warning, one alarm), even though the
/// live sweeper never ran.
#[tokio::test]
#[serial]
async fn csv_import_triggers_alarm_backfill_with_warnings_and_alarms() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    // Seeds SITE1/Turbidity site_parameter + a global threshold (warning > 100, alarm > 500).
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    assert!(
        turbidity_episodes(&db).await.is_empty(),
        "no alarm events should exist before the import"
    );

    // 50 ok → 150 warning → 40 ok (resolves the warning) → 600 alarm → 30 ok (resolves the alarm).
    let csv = "DateTime,Turbidity\n\
        2025-02-01 00:00:00,50\n\
        2025-02-01 00:10:00,150\n\
        2025-02-01 00:20:00,40\n\
        2025-02-01 00:30:00,600\n\
        2025-02-01 00:40:00,30\n";

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": crate::common::SITE1_ID, "csv": csv }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import_csv ({status}): {body}");
    assert_eq!(
        body["inserted_total"].as_u64(),
        Some(5),
        "all five rows should map to Turbidity and insert: {body}"
    );

    let job_status = wait_for_alarm_backfill(&db).await;
    assert_eq!(job_status, "completed", "alarm_backfill job should complete");

    // Two resolved episodes: a warning (max_severity 1) then an alarm (max_severity 2).
    let episodes = turbidity_episodes(&db).await;
    assert_eq!(
        episodes,
        vec![(1, true), (2, true)],
        "expected one resolved warning then one resolved alarm episode, got {episodes:?}"
    );

    // The history feed surfaces them.
    let (status, events) = crate::common::get_json_with_token(
        &app,
        &format!("/api/alarms/events?site_id={}", crate::common::SITE1_ID),
        &token,
    )
    .await;
    assert_eq!(status, 200, "alarms/events ({status}): {events}");
    let turb_events: Vec<&serde_json::Value> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["parameter_id"].as_str() == Some(crate::common::GLOBAL_PARAM_TURB_ID))
        .collect();
    assert_eq!(turb_events.len(), 2, "two events in the feed: {events}");
    assert!(
        turb_events.iter().any(|e| e["max_severity"].as_i64() == Some(2)),
        "an alarm-severity event is present: {events}"
    );
    assert!(
        turb_events.iter().any(|e| e["max_severity"].as_i64() == Some(1)),
        "a warning-severity event is present: {events}"
    );
}

/// The on-demand `POST /actions/rebuild_alarm_events` reconstructs episodes for a window and is
/// idempotent: running it twice yields exactly the same set of events (delete-then-reinsert).
#[tokio::test]
#[serial]
async fn rebuild_alarm_events_action_is_idempotent() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let site1 = crate::common::SITE1_ID;
    let turb = crate::common::GLOBAL_PARAM_TURB_ID;
    let stream_id: Uuid = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT stream_id FROM readings WHERE site_id='{site1}' AND parameter_id='{turb}' LIMIT 1"),
        ))
        .await
        .unwrap()
        .expect("a seeded turbidity stream")
        .try_get("", "stream_id")
        .unwrap();

    // Inject a breach run that resolves, so the rebuild produces a closed episode.
    for (time, value) in [
        ("2025-02-01T00:00:00Z", 50.0),
        ("2025-02-01T00:10:00Z", 600.0),
        ("2025-02-01T00:20:00Z", 40.0),
    ] {
        db.execute(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
                 VALUES ('{stream_id}', '{site1}', '{turb}', '{time}', {value}, 0) ON CONFLICT DO NOTHING"
            ),
        ))
        .await
        .unwrap();
    }

    let rebuild = || async {
        let (status, body) = crate::common::post_json_with_token(
            &app,
            "/api/actions/rebuild_alarm_events",
            &serde_json::json!({
                "site_id": site1,
                "parameter_id": turb,
                "start": "2025-02-01T00:00:00Z",
                "end": "2025-02-01T00:40:00Z",
            }),
            &token,
        )
        .await;
        assert!((200..300).contains(&status), "rebuild ({status}): {body}");
        wait_for_alarm_backfill(&db).await
    };

    assert_eq!(rebuild().await, "completed");
    let first = turbidity_episodes(&db).await;
    assert_eq!(
        first,
        vec![(2, true)],
        "one resolved alarm episode after first rebuild, got {first:?}"
    );

    // Running it again must not duplicate or drop episodes.
    assert_eq!(rebuild().await, "completed");
    let second = turbidity_episodes(&db).await;
    assert_eq!(second, first, "rebuild is idempotent: {second:?} vs {first:?}");
}
