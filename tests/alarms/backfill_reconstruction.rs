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
use uuid::Uuid;

/// (max_severity, resolved) for every alarm event at SITE1/Turbidity, ordered by start.
async fn turbidity_episodes(db: &sea_orm::DatabaseConnection) -> Vec<(i16, bool)> {
    db.query_all_raw(Statement::from_string(
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

    let job_status = crate::common::jobs::wait_for_triggered_job(&db, "alarm_backfill", None).await;
    assert_eq!(
        job_status, "completed",
        "alarm_backfill job should complete"
    );

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
        turb_events
            .iter()
            .any(|e| e["max_severity"].as_i64() == Some(2)),
        "an alarm-severity event is present: {events}"
    );
    assert!(
        turb_events
            .iter()
            .any(|e| e["max_severity"].as_i64() == Some(1)),
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
        .query_one_raw(Statement::from_string(
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
        db.execute_raw(Statement::from_string(
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
        crate::common::jobs::wait_for_triggered_job(&db, "alarm_backfill", None).await
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
    assert_eq!(
        second, first,
        "rebuild is idempotent: {second:?} vs {first:?}"
    );
}

/// (id, acknowledged_by, resolution_notified_at IS NOT NULL) for every threshold episode at
/// SITE1/Turbidity, ordered by start.
async fn turbidity_episode_state(
    db: &sea_orm::DatabaseConnection,
) -> Vec<(Uuid, Option<String>, bool)> {
    db.query_all_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT id, acknowledged_by, resolution_notified_at IS NOT NULL AS told \
             FROM alarm_events WHERE site_id = '{site}' AND parameter_id = '{param}' \
             AND kind = 'threshold' ORDER BY started_at",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    ))
    .await
    .unwrap()
    .into_iter()
    .map(|r| {
        (
            r.try_get::<Uuid>("", "id").unwrap(),
            r.try_get::<Option<String>>("", "acknowledged_by").unwrap(),
            r.try_get::<bool>("", "told").unwrap(),
        )
    })
    .collect()
}

/// Scenario: a manager acknowledged a resolved episode and its subscribers were told it closed,
/// then the history is rebuilt over the same window, once unchanged and once with a later
/// breach backfilled beside it.
///
/// Expected behaviour: the stored episode keeps its id, its acknowledgement and its notification
/// state; only the backfilled breach is added.
#[tokio::test]
#[serial]
async fn rebuild_keeps_an_acknowledged_episode_in_place() {
    use river_db::routes::private::alarms::flows::evaluate_alarm_episodes;

    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    let site1 = crate::common::SITE1_ID;
    let turb = crate::common::GLOBAL_PARAM_TURB_ID;
    let stream_id: Uuid = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT stream_id FROM readings WHERE site_id='{site1}' AND parameter_id='{turb}' LIMIT 1"),
        ))
        .await
        .unwrap()
        .expect("a seeded turbidity stream")
        .try_get("", "stream_id")
        .unwrap();
    let inject = |time: &'static str, value: f64| {
        let db = db.clone();
        async move {
            crate::common::exec(
                &db,
                &format!(
                    "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
                     VALUES ('{stream_id}', '{site1}', '{turb}', '{time}', {value}, 0) ON CONFLICT DO NOTHING"
                ),
            )
            .await;
        }
    };
    let start = "2025-02-01T00:00:00Z".parse().unwrap();
    let end = "2025-02-01T01:00:00Z".parse().unwrap();

    inject("2025-02-01T00:00:00Z", 50.0).await;
    inject("2025-02-01T00:10:00Z", 600.0).await;
    inject("2025-02-01T00:20:00Z", 40.0).await;
    evaluate_alarm_episodes(
        &db,
        site1.parse().unwrap(),
        turb.parse().unwrap(),
        start,
        end,
    )
    .await
    .unwrap();
    let [(id, None, false)] = turbidity_episode_state(&db).await[..] else {
        panic!("one unacknowledged resolved episode after the first rebuild");
    };
    crate::common::exec(
        &db,
        &format!(
            "UPDATE alarm_events SET acknowledged_at = now(), acknowledged_by = 'manager', \
             notified_at = now(), resolution_notified_at = now() WHERE id = '{id}'"
        ),
    )
    .await;

    evaluate_alarm_episodes(
        &db,
        site1.parse().unwrap(),
        turb.parse().unwrap(),
        start,
        end,
    )
    .await
    .unwrap();
    assert_eq!(
        turbidity_episode_state(&db).await,
        vec![(id, Some("manager".to_string()), true)],
        "an unchanged rebuild leaves the acknowledged episode as it was"
    );

    inject("2025-02-01T00:30:00Z", 150.0).await;
    inject("2025-02-01T00:40:00Z", 30.0).await;
    evaluate_alarm_episodes(
        &db,
        site1.parse().unwrap(),
        turb.parse().unwrap(),
        start,
        end,
    )
    .await
    .unwrap();
    let after = turbidity_episode_state(&db).await;
    assert_eq!(after.len(), 2, "the backfilled breach is added: {after:?}");
    assert_eq!(
        after[0],
        (id, Some("manager".to_string()), true),
        "the acknowledged episode keeps its row"
    );
    assert_eq!(after[1].1, None, "the new episode is unacknowledged");
}
