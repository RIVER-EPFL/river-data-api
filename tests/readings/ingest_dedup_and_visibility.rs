//! Ingestion idempotency + visibility gaps the doc promises but the suite didn't assert:
//! duplicate readings/status-events are first-write-wins on the hypertable PK, the batch endpoint's
//! skip-vs-overwrite conflict modes, and unpaired-stream readings stay out of continuous aggregates
//! until the stream is paired.
//!
//! Run: cargo test --test readings -- --test-threads=1

use crate::common::e2e;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            sql.to_string(),
        ))
        .await
        .expect("query")
        .expect("row");
    row.try_get::<i64>("", "c").expect("c")
}

async fn register_stream(app: &axum::Router, token: &str, key: &str) -> String {
    let (status, stream) = crate::common::post_json_parse_with_token(
        app,
        "/api/streams/register",
        &serde_json::json!({"source_system": "dedup", "source_key": key}),
        token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "register ({status}): {stream}"
    );
    e2e::id_of(&stream)
}

#[tokio::test]
#[serial]
async fn ingest_duplicate_reading_first_write_wins() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let stream = register_stream(&app, &token, "r1").await;

    let t = "2025-01-15T00:00:00Z";
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &serde_json::json!({"stream_id": stream, "readings": [{"time": t, "raw_value": 10.0}]}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "first ingest ({status}): {body}");
    assert_eq!(body["inserted"], 1);

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &serde_json::json!({"stream_id": stream, "readings": [{"time": t, "raw_value": 99.0}]}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "dup ingest ({status}): {body}");
    assert_eq!(
        body["inserted"], 0,
        "duplicate (stream,time,replicate) skipped"
    );

    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) AS c FROM readings WHERE stream_id = '{stream}'")
        )
        .await,
        1,
        "only one row stored"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings WHERE stream_id = '{stream}' AND raw_value = 10"
            )
        )
        .await,
        1,
        "first write wins"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings WHERE stream_id = '{stream}' AND raw_value = 99"
            )
        )
        .await,
        0,
        "second write dropped"
    );
}

#[tokio::test]
#[serial]
async fn ingest_status_event_dedup_first_write_wins() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let stream = register_stream(&app, &token, "se1").await;

    // The series keeps its first value and its transitions: a repeated "still ok" poll says
    // nothing and is not stored.
    let t1 = "2025-01-15T00:00:00Z";
    let t2 = "2025-01-15T01:00:00Z";
    let t3 = "2025-01-15T02:00:00Z";
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest/status_events",
        &serde_json::json!({"stream_id": stream, "events": [
            {"time": t1, "value": "ok"}, {"time": t2, "value": "ok"}, {"time": t3, "value": "ok"}
        ]}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "first status ingest ({status}): {body}");
    assert_eq!(
        body["inserted"], 1,
        "repeats collapse to the first value: {body}"
    );

    // A value change past the stored tip lands; a re-sent stored timestamp keeps its first
    // value; a repeat of the latest value is dropped again.
    let t4 = "2025-01-15T03:00:00Z";
    let (status, body) = crate::common::post_json_parse_with_token(
        &app, "/api/ingest/status_events",
        &serde_json::json!({"stream_id": stream, "events": [
            {"time": t1, "value": "changed"}, {"time": t2, "value": "changed"}, {"time": t4, "value": "changed"}
        ]}),
        &token,
    ).await;
    assert_eq!(status, 200, "overlapping status ingest ({status}): {body}");

    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) AS c FROM status_events WHERE stream_id = '{stream}'")
        )
        .await,
        2,
        "the transition landed once; the tip repeat was dropped"
    );
    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM status_events WHERE stream_id = '{stream}' AND time = '{t1}' AND value = 'ok'")).await,
        1, "existing (stream,time) keeps its first value"
    );
    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM status_events WHERE stream_id = '{stream}' AND time = '{t2}' AND value = 'changed'")).await,
        1, "the transition is recorded at its first instant"
    );
}

#[tokio::test]
#[serial]
async fn batch_reading_conflict_skip_then_overwrite() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let project = e2e::create_project(&app, &token, "Batch P", "batch-p", false).await;
    let site = e2e::create_site(&app, &token, &project, "Batch Site", "batch-site").await;
    let param = e2e::create_parameter(&app, &token, "batchcond", "Conductivity", "uS/cm").await;
    // The slot the readings belong to: attribution comes from the pairing, so a parameter the site
    // has not been assigned would store its readings unattributed.
    e2e::assign_site_parameter_minimal(&app, &token, &site, &param).await;
    let t = "2025-01-15T00:00:00Z";

    let body = serde_json::json!({"readings": [
        {"site_id": site, "parameter_id": param, "time": t, "raw_value": 10.0}
    ]});
    let (status, resp) =
        crate::common::post_json_parse_with_token(&app, "/api/readings/batch", &body, &token).await;
    assert_eq!(status, 200, "first batch ({status}): {resp}");
    assert_eq!(resp["inserted"], 1);

    // default conflict = skip → first write wins
    let body = serde_json::json!({"readings": [
        {"site_id": site, "parameter_id": param, "time": t, "raw_value": 99.0}
    ]});
    let (status, resp) =
        crate::common::post_json_parse_with_token(&app, "/api/readings/batch", &body, &token).await;
    assert_eq!(status, 200, "skip batch ({status}): {resp}");
    assert_eq!(resp["inserted"], 0, "collision skipped by default");
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings WHERE site_id = '{site}' AND raw_value = 10"
            )
        )
        .await,
        1,
        "skip kept the original value"
    );

    // explicit overwrite → value replaced
    let body = serde_json::json!({"readings": [
        {"site_id": site, "parameter_id": param, "time": t, "raw_value": 77.0}
    ], "conflict": "overwrite"});
    let (status, resp) =
        crate::common::post_json_parse_with_token(&app, "/api/readings/batch", &body, &token).await;
    assert_eq!(status, 200, "overwrite batch ({status}): {resp}");
    assert_eq!(resp["overwritten"], 1, "overwrite replaced the row");
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings WHERE site_id = '{site}' AND raw_value = 77"
            )
        )
        .await,
        1,
        "overwrite stored the new value"
    );
}

#[tokio::test]
#[serial]
async fn unpaired_readings_excluded_from_aggregates_until_paired() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let project = e2e::create_project(&app, &token, "Agg P", "agg-p", false).await;
    let site = e2e::create_site(&app, &token, &project, "Agg Site", "agg-site").await;
    let param = e2e::create_parameter(&app, &token, "aggcond", "Conductivity", "uS/cm").await;
    let sp = e2e::assign_site_parameter_minimal(&app, &token, &site, &param).await;
    let stream = register_stream(&app, &token, "agg1").await;

    // Ingest onto the UNPAIRED stream, readings carry site_id = NULL.
    let readings: Vec<serde_json::Value> = (0..6)
        .map(|i| serde_json::json!({"time": format!("2025-01-15T0{i}:00:00Z"), "raw_value": 100.0 + i as f64}))
        .collect();
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &serde_json::json!({"stream_id": stream, "readings": readings}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "ingest ({status}): {body}");
    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM readings WHERE stream_id = '{stream}' AND site_id IS NULL")).await,
        6, "unpaired readings have NULL site_id"
    );

    crate::common::refresh_continuous_aggregates(&db).await;
    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) AS c FROM readings_hourly WHERE site_id = '{site}'")
        )
        .await,
        0,
        "unpaired readings are excluded from the continuous aggregate"
    );

    // Pair the stream → backfill stamps site_id onto the existing readings.
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{stream}/pair"),
        &serde_json::json!({"site_parameter_id": sp}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "pair ({status}): {body}");
    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM readings WHERE stream_id = '{stream}' AND site_id = '{site}'")).await,
        6, "pairing backfilled site_id"
    );

    crate::common::refresh_continuous_aggregates(&db).await;
    assert!(
        count(
            &db,
            &format!("SELECT count(*) AS c FROM readings_hourly WHERE site_id = '{site}'")
        )
        .await
            > 0,
        "after pairing the readings appear in the continuous aggregate"
    );
}

#[tokio::test]
#[serial]
async fn ingest_overwrite_updates_values_and_is_sync_only() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (sync_token, _service_id) = crate::common::seed_sync_session_token(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let stream = register_stream(&app, &token, "ow1").await;

    let t = "2025-01-15T00:00:00Z";
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &serde_json::json!({"stream_id": stream, "readings": [{"time": t, "raw_value": 10.0}]}),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "first ingest ({status}): {body}");

    db.execute_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "UPDATE readings SET is_flagged = TRUE, flag_reason = 'manual' WHERE stream_id = '{stream}'"
        ),
    ))
    .await
    .expect("flag reading");

    let (status, body) = crate::common::post_json_parse_with_token(
        &app, "/api/ingest",
        &serde_json::json!({"stream_id": stream, "overwrite": true, "readings": [{"time": t, "raw_value": 42.5}]}),
        &sync_token,
    ).await;
    assert_eq!(status, 200, "overwrite ingest ({status}): {body}");

    assert_eq!(
        count(&db, &format!("SELECT count(*) AS c FROM readings WHERE stream_id = '{stream}' AND raw_value = 42.5")).await,
        1, "correction applied in place"
    );
    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) AS c FROM readings WHERE stream_id = '{stream}'")
        )
        .await,
        1,
        "still one row"
    );
    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM readings WHERE stream_id = '{stream}' AND is_flagged AND flag_reason = 'manual'"
        )).await,
        1, "operator flag survives the overwrite"
    );

    let (status, _) = crate::common::post_json_with_token(
        &app, "/api/ingest",
        &serde_json::json!({"stream_id": stream, "overwrite": true, "readings": [{"time": t, "raw_value": 1.0}]}),
        &token,
    ).await;
    assert_eq!(status, 403, "overwrite is refused for API tokens");
}

/// A standard curve is chosen by hand per measurement and no query can recover it, so a source-side
/// correction must leave it standing and the value it serves must come back through it.
#[tokio::test]
#[serial]
async fn ingest_overwrite_keeps_a_hand_picked_curve_and_recomposes_through_it() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (sync_token, _service_id) = crate::common::seed_sync_session_token(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let stream = register_stream(&app, &token, "curve-survives").await;

    let t = "2025-01-15T00:00:00Z";
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &serde_json::json!({"stream_id": stream, "readings": [{"time": t, "raw_value": 10.0}]}),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "first ingest ({status}): {body}");

    let sensor_id = "00000000-0000-4000-c000-0000000000c1";
    let curve_id = "00000000-0000-4000-c000-0000000000d1";
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, is_active, is_lab_instrument, created_at) \
             VALUES ('{sensor_id}', 'Bench reader', true, true, now())"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept, name) \
             VALUES ('{curve_id}', '{sensor_id}', 3.0, 0.5, 'Plate A')"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET standard_curve_id = '{curve_id}', calibrated_value = 30.5 \
             WHERE stream_id = '{stream}' AND time = '{t}'"
        ),
    )
    .await;

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &serde_json::json!({
            "stream_id": stream, "overwrite": true,
            "readings": [{"time": t, "raw_value": 42.5}]
        }),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "the correction is accepted ({status}): {body}");

    let row = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT raw_value, calibrated_value, standard_curve_id FROM readings \
                 WHERE stream_id = '{stream}' AND time = '{t}'"
            ),
        ))
        .await
        .expect("query")
        .expect("the corrected reading is stored");
    assert_eq!(
        row.try_get::<f64>("", "raw_value").unwrap(),
        42.5,
        "the correction replaced the measurement"
    );
    assert_eq!(
        row.try_get::<Option<uuid::Uuid>>("", "standard_curve_id")
            .unwrap()
            .map(|id| id.to_string()),
        Some(curve_id.to_string()),
        "a re-send naming no curve leaves the operator's standing"
    );
    assert_eq!(
        row.try_get::<Option<f64>>("", "calibrated_value").unwrap(),
        Some(128.0),
        "3 * 42.5 + 0.5: the served value comes back through that curve"
    );
}

/// Expected behaviour: a device status is attributed to the instrument its stream names, the way
/// a reading on the same stream is. The Vaisala service cannot name one, since the stream list it
/// reads carries no instrument, and a stream carries none until it is paired (M172), so the
/// pairing is what puts an instrument on the channel for both arms to take.
#[tokio::test]
#[serial]
async fn ingest_status_event_takes_the_stream_instrument() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    crate::common::seed_test_data(&db).await;
    let stream = register_stream(&app, &token, "se-instrument").await;
    let (status, paired) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{stream}/pair"),
        &serde_json::json!({ "site_parameter_id": crate::common::PARAM_S1_TEMP_ID }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "pair ({status}): {paired}");

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest/status_events",
        &serde_json::json!({"stream_id": stream, "events": [
            {"time": "2025-02-01T00:00:00Z", "value": "ok"}
        ]}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "status ingest ({status}): {body}");

    assert_eq!(attributed(&db, &stream).await, 1, "the sync arm");

    // The batch arm mints its own "api" channel per slot, and that channel carries an instrument
    // too.
    let project =
        e2e::create_project(&app, &token, "Status Instrument", "status-instr", false).await;
    let site = e2e::create_site(&app, &token, &project, "Status Site", "status-site").await;
    let parameter =
        e2e::create_parameter(&app, &token, "StatInstr", "Status Instrument", "state").await;
    e2e::assign_site_parameter_minimal(&app, &token, &site, &parameter).await;
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/status_events/batch",
        &serde_json::json!({"events": [
            {"site_id": site, "parameter_id": parameter,
             "time": "2025-02-01T00:00:00Z", "value": "ok"}
        ]}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "batch status events ({status}): {body}");

    let api_stream = format!("{site}:{parameter}");
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM status_events e JOIN data_streams s ON s.id = e.stream_id \
                 WHERE s.source_system = 'api' AND s.source_key = '{api_stream}' \
                 AND e.sensor_id IS NOT DISTINCT FROM s.sensor_id AND e.sensor_id IS NOT NULL"
            )
        )
        .await,
        1,
        "the batch arm"
    );
}

/// Expected behaviour: a batch status event names a site and parameter, and is attributed to them
/// only when the site carries that slot, as a batch reading is. A parameter the site was never
/// assigned leaves the event on its unpaired api channel with no site and no parameter.
#[tokio::test]
#[serial]
async fn batch_status_event_without_a_slot_is_stored_unattributed() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let project = e2e::create_project(&app, &token, "Status Slot", "status-slot", false).await;
    let site = e2e::create_site(&app, &token, &project, "Slot Site", "slot-site").await;
    let assigned = e2e::create_parameter(&app, &token, "SlotOn", "Slot assigned", "state").await;
    let unassigned = e2e::create_parameter(&app, &token, "SlotOff", "Slot missing", "state").await;
    e2e::assign_site_parameter_minimal(&app, &token, &site, &assigned).await;

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/status_events/batch",
        &serde_json::json!({"events": [
            {"site_id": site, "parameter_id": assigned,
             "time": "2025-02-01T00:00:00Z", "value": "ok"},
            {"site_id": site, "parameter_id": unassigned,
             "time": "2025-02-01T00:00:00Z", "value": "offline"}
        ]}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "batch status events ({status}): {body}");

    let attributed_on = |parameter: &str| {
        format!(
            "SELECT count(*) AS c FROM status_events e JOIN data_streams s ON s.id = e.stream_id \
             WHERE s.source_system = 'api' AND s.source_key = '{site}:{parameter}' \
             AND e.site_id IS NOT NULL AND e.parameter_id IS NOT NULL"
        )
    };
    assert_eq!(
        count(&db, &attributed_on(&assigned)).await,
        1,
        "the slot backs the event"
    );
    assert_eq!(
        count(&db, &attributed_on(&unassigned)).await,
        0,
        "no slot backs the event"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM status_events e JOIN data_streams s ON s.id = e.stream_id \
                 WHERE s.source_key = '{site}:{unassigned}' AND s.site_parameter_id IS NULL \
                 AND e.site_id IS NULL AND e.parameter_id IS NULL"
            )
        )
        .await,
        1,
        "the event is stored on its unpaired channel"
    );
}

/// Scenario: a batch writes a site and parameter before the site carries that slot, an admin adds
/// the slot, and later unpairs the api channel.
/// Expected behaviour: every batch after the slot exists pairs the channel to it, with the backfill
/// a pairing owes, so the stored readings and the channel's pairing agree at every step.
#[tokio::test]
#[serial]
async fn batch_pairs_its_api_channel_once_the_slot_exists() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let project = e2e::create_project(&app, &token, "Late Slot", "late-slot", false).await;
    let site = e2e::create_site(&app, &token, &project, "Late Site", "late-site").await;
    let param = e2e::create_parameter(&app, &token, "lateslot", "Late slot", "m").await;
    let batch = |time: &str| {
        serde_json::json!({"readings": [
            {"site_id": site, "parameter_id": param, "time": time, "raw_value": 1.0}
        ]})
    };
    let post = async |time: &str| {
        let (status, body) = crate::common::post_json_parse_with_token(
            &app,
            "/api/readings/batch",
            &batch(time),
            &token,
        )
        .await;
        assert_eq!(status, 200, "batch at {time} ({status}): {body}");
    };
    let channel = format!("s.source_system = 'api' AND s.source_key = '{site}:{param}'");
    let disagreeing = format!(
        "SELECT count(*) AS c FROM readings r JOIN data_streams s ON s.id = r.stream_id \
         WHERE {channel} AND ((r.site_id IS NULL) <> (s.site_parameter_id IS NULL))"
    );
    let attributed = format!(
        "SELECT count(*) AS c FROM readings r JOIN data_streams s ON s.id = r.stream_id \
         WHERE {channel} AND r.site_id = '{site}' AND r.parameter_id = '{param}'"
    );

    post("2025-03-01T00:00:00Z").await;
    assert_eq!(
        count(&db, &attributed).await,
        0,
        "no slot backs the first row"
    );
    assert_eq!(
        count(&db, &disagreeing).await,
        0,
        "unpaired channel, unattributed row"
    );

    e2e::assign_site_parameter_minimal(&app, &token, &site, &param).await;
    post("2025-03-02T00:00:00Z").await;
    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) AS c FROM data_streams s WHERE {channel} AND s.site_parameter_id IS NOT NULL")
        )
        .await,
        1,
        "the batch paired its channel to the slot"
    );
    assert_eq!(
        count(&db, &disagreeing).await,
        0,
        "rows agree with the pairing"
    );
    assert_eq!(
        count(&db, &attributed).await,
        2,
        "the pairing backfilled the first row"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM reprocessing_jobs j JOIN data_streams s ON s.id = j.trigger_id \
                 WHERE {channel} AND j.trigger_type = 'pairing_backfill'"
            )
        )
        .await,
        1,
        "the pairing queued the slot reprocess"
    );

    let stream = {
        let row = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("SELECT s.id::text AS id FROM data_streams s WHERE {channel}"),
            ))
            .await
            .expect("query")
            .expect("row");
        row.try_get::<String>("", "id").expect("id")
    };
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{stream}/unpair"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "unpair ({status}): {body}");
    assert_eq!(
        count(&db, &disagreeing).await,
        0,
        "unpair released every row"
    );

    post("2025-03-03T00:00:00Z").await;
    assert_eq!(
        count(&db, &disagreeing).await,
        0,
        "rows agree with the re-pairing"
    );
    assert_eq!(
        count(&db, &attributed).await,
        3,
        "the re-pairing backfilled every row"
    );
}

/// Scenario: an instrument is deployed at a site and parameter the site carries no slot for, a
/// batch writes a reading there, and an admin later adds the slot.
/// Expected behaviour: the reading lands on the unpaired api channel with no instrument,
/// deployment, curve or corrected value, since no pairing decided any of them, and the pairing the
/// next batch makes stamps all four from the deployment.
#[tokio::test]
#[serial]
async fn batch_on_an_unpaired_channel_derives_no_attribution() {
    use crate::common::sensor_lifecycle::{
        add_calibration_for_parameter, create_sensor_without_curve, deploy_sensor_for_parameter, dt,
    };
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let project = e2e::create_project(&app, &token, "No Slot", "no-slot", false).await;
    let site = e2e::create_site(&app, &token, &project, "No Slot Site", "no-slot").await;
    let param = e2e::create_parameter(&app, &token, "noslot", "No slot", "m").await;
    let sensor = create_sensor_without_curve(&db, "Deployed without a slot").await;
    let calibration =
        add_calibration_for_parameter(&db, sensor, &param, 2.0, 0.0, dt("2025-01-01T00:00:00Z"))
            .await;
    let deployment =
        deploy_sensor_for_parameter(&db, sensor, &site, &param, dt("2025-01-01T00:00:00Z")).await;
    let post = async |time: &str| {
        let (status, body) = crate::common::post_json_parse_with_token(
            &app,
            "/api/readings/batch",
            &serde_json::json!({"readings": [
                {"site_id": site, "parameter_id": param, "time": time, "raw_value": 1.0}
            ]}),
            &token,
        )
        .await;
        assert_eq!(status, 200, "batch at {time} ({status}): {body}");
    };
    let channel = format!("s.source_system = 'api' AND s.source_key = '{site}:{param}'");
    let first = |predicate: &str| {
        format!(
            "SELECT count(*) AS c FROM readings r JOIN data_streams s ON s.id = r.stream_id \
             WHERE {channel} AND r.time = '2025-03-01T00:00:00Z' AND {predicate}"
        )
    };

    post("2025-03-01T00:00:00Z").await;
    assert_eq!(
        count(
            &db,
            &first(
                "r.site_id IS NULL AND r.sensor_id IS NULL AND r.deployment_id IS NULL \
                 AND r.calibration_id IS NULL AND r.calibrated_value IS NULL"
            )
        )
        .await,
        1,
        "the row on the unpaired channel is staged with nothing derived"
    );

    e2e::assign_site_parameter_minimal(&app, &token, &site, &param).await;
    post("2025-03-02T00:00:00Z").await;
    let stream = {
        let row = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("SELECT s.id::text AS id FROM data_streams s WHERE {channel}"),
            ))
            .await
            .expect("query")
            .expect("row");
        row.try_get::<String>("", "id").expect("id")
    };
    assert_eq!(
        crate::common::jobs::wait_for_triggered_job(&db, "pairing_backfill", Some(&stream)).await,
        "completed"
    );
    assert_eq!(
        count(
            &db,
            &first(&format!(
                "r.site_id = '{site}' AND r.sensor_id = '{sensor}' \
                 AND r.deployment_id = '{deployment}' AND r.calibration_id = '{calibration}' \
                 AND r.calibrated_value = 2.0"
            ))
        )
        .await,
        1,
        "the pairing stamps the instrument, deployment and curve the slot's deployment names"
    );
}

/// Scenario: status events are batched for a site and parameter before the site carries that slot,
/// an admin adds the slot, and later unpairs the api channel.
/// Expected behaviour: every batch after the slot exists pairs the channel to it, with the backfill
/// a pairing owes, so the stored events and the channel's pairing agree at every step.
#[tokio::test]
#[serial]
async fn batch_status_events_pair_their_api_channel_once_the_slot_exists() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let project = e2e::create_project(&app, &token, "Late Status", "late-status", false).await;
    let site = e2e::create_site(&app, &token, &project, "Late Status Site", "late-status").await;
    let param = e2e::create_parameter(&app, &token, "latestate", "Late state", "state").await;
    let post = async |time: &str| {
        let (status, body) = crate::common::post_json_parse_with_token(
            &app,
            "/api/status_events/batch",
            &serde_json::json!({"events": [
                {"site_id": site, "parameter_id": param, "time": time, "value": "ok"}
            ]}),
            &token,
        )
        .await;
        assert_eq!(status, 200, "batch at {time} ({status}): {body}");
    };
    let channel = format!("s.source_system = 'api' AND s.source_key = '{site}:{param}'");
    let disagreeing = format!(
        "SELECT count(*) AS c FROM status_events e JOIN data_streams s ON s.id = e.stream_id \
         WHERE {channel} AND ((e.site_id IS NULL) <> (s.site_parameter_id IS NULL))"
    );
    let attributed = format!(
        "SELECT count(*) AS c FROM status_events e JOIN data_streams s ON s.id = e.stream_id \
         WHERE {channel} AND e.site_id = '{site}' AND e.parameter_id = '{param}'"
    );
    let paired = format!(
        "SELECT count(*) AS c FROM data_streams s WHERE {channel} AND s.site_parameter_id IS NOT NULL"
    );

    post("2025-03-01T00:00:00Z").await;
    assert_eq!(
        count(&db, &attributed).await,
        0,
        "no slot backs the first event"
    );
    assert_eq!(
        count(&db, &disagreeing).await,
        0,
        "unpaired channel, unattributed event"
    );

    e2e::assign_site_parameter_minimal(&app, &token, &site, &param).await;
    post("2025-03-02T00:00:00Z").await;
    assert_eq!(
        count(&db, &paired).await,
        1,
        "the batch paired its channel to the slot"
    );
    assert_eq!(
        count(&db, &disagreeing).await,
        0,
        "events agree with the pairing"
    );
    assert_eq!(
        count(&db, &attributed).await,
        2,
        "the pairing backfilled the first event"
    );

    let stream = {
        let row = db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("SELECT s.id::text AS id FROM data_streams s WHERE {channel}"),
            ))
            .await
            .expect("query")
            .expect("row");
        row.try_get::<String>("", "id").expect("id")
    };
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{stream}/unpair"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "unpair ({status}): {body}");
    assert_eq!(
        count(&db, &disagreeing).await,
        0,
        "unpair released every event"
    );

    post("2025-03-03T00:00:00Z").await;
    assert_eq!(
        count(&db, &paired).await,
        1,
        "the batch re-paired its channel"
    );
    assert_eq!(
        count(&db, &disagreeing).await,
        0,
        "events agree with the re-pairing"
    );
    assert_eq!(
        count(&db, &attributed).await,
        3,
        "the re-pairing backfilled every event"
    );
}

/// Status events on a stream that carry exactly the instrument that stream names.
async fn attributed(db: &DatabaseConnection, stream: &str) -> i64 {
    count(
        db,
        &format!(
            "SELECT count(*) AS c FROM status_events e JOIN data_streams s ON s.id = e.stream_id \
             WHERE e.stream_id = '{stream}' AND e.sensor_id IS NOT DISTINCT FROM s.sensor_id \
             AND e.sensor_id IS NOT NULL"
        ),
    )
    .await
}
