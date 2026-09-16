//! Collection events (D7): every attributed spot instant belongs to one
//! `(site, collected_at)` event, whatever path wrote it. A grab entry attaches a manual event, a
//! sync-service ingest a portal_sync one, a pairing backfill attaches late, and the staging
//! endpoint is the portal's New Entry.
//!
//! Run: cargo test --test readings collection_events -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement, TransactionTrait};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_DO_ID, GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const T1: &str = "2025-06-01T08:00:00Z";

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

async fn event_row(db: &DatabaseConnection, time: &str) -> Option<(String, String)> {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT id::text AS id, source FROM collection_events \
             WHERE site_id = '{SITE1_ID}' AND collected_at = '{time}'"
        ),
    ))
    .await
    .unwrap()
    .map(|r| {
        (
            r.try_get::<String>("", "id").unwrap(),
            r.try_get::<String>("", "source").unwrap(),
        )
    })
}

#[tokio::test]
#[serial]
async fn a_grab_save_attaches_a_manual_event_to_its_readings() {
    let (db, app, token) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "readings": [
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 10.0, "time": T1 },
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 12.0, "time": T1 },
                { "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 4.2, "time": T1 },
            ],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let (event_id, source) = event_row(&db, T1).await.expect("one event per instant");
    assert_eq!(source, "manual");
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*) AS n FROM readings \
                 WHERE collection_event_id = '{event_id}' AND time = '{T1}'"
            ),
        )
        .await,
        3,
        "both parameters' replicates share the one visit"
    );

    // A second save at the same instant reuses the event rather than minting a sibling.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "readings": [{ "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 5.0, "time": T1 }],
            "mode": "replace",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*) AS n FROM collection_events \
                 WHERE site_id = '{SITE1_ID}' AND collected_at = '{T1}'"
            ),
        )
        .await,
        1
    );
}

#[tokio::test]
#[serial]
async fn a_sync_ingest_attaches_a_portal_sync_event() {
    let (db, app, token) = setup().await;
    let (sync_token, _service) = crate::common::seed_sync_session_token(&db).await;

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({"source_system": "cnet", "source_key": "stn:DOC_avg:reps", "measurement_type": "spot"}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "{stream}");
    let stream_id = crate::common::e2e::id_of(&stream);
    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({"site_parameter_id": crate::common::PARAM_S1_DO_ID}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/ingest",
        &json!({
            "stream_id": stream_id,
            "collection": true,
            "readings": [
                { "time": T1, "raw_value": 118.0, "replicate_index": 0 },
                { "time": T1, "raw_value": 122.0, "replicate_index": 1 },
            ],
        }),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let (event_id, source) = event_row(&db, T1)
        .await
        .expect("the portal row became a visit");
    assert_eq!(source, "portal_sync");
    assert_eq!(
        scalar_i64(
            &db,
            &format!("SELECT COUNT(*) AS n FROM readings WHERE collection_event_id = '{event_id}'"),
        )
        .await,
        2
    );
}

#[tokio::test]
#[serial]
async fn pairing_attaches_events_to_a_backfilled_stream() {
    let (db, app, token) = setup().await;
    let (sync_token, _service) = crate::common::seed_sync_session_token(&db).await;

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({"source_system": "metalp", "source_key": "stn:late:reps", "measurement_type": "spot"}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "{stream}");
    let stream_id = crate::common::e2e::id_of(&stream);

    // Unpaired ingest: no site, so no event yet.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/ingest",
        &json!({
            "stream_id": stream_id,
            "collection": true,
            "readings": [
                { "time": T1, "raw_value": 1.0, "replicate_index": 0 },
                { "time": T1, "raw_value": 2.0, "replicate_index": 1 },
            ],
        }),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(event_row(&db, T1).await.is_none());

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({"site_parameter_id": crate::common::PARAM_S1_DO_ID}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let (_, source) = event_row(&db, T1)
        .await
        .expect("pairing attached the visit");
    assert_eq!(
        source, "portal_sync",
        "a portal stream's backfill is a synced visit"
    );
}

#[tokio::test]
#[serial]
async fn the_staging_endpoint_creates_a_manual_event() {
    let (db, app, token) = setup().await;

    let (status, event) = crate::common::post_json_parse_with_token(
        &app,
        "/api/collection_events",
        &json!({
            "site_id": SITE1_ID,
            "collected_at": T1,
            "created_by": "field@example.org",
            "notes": "spring campaign",
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "stage ({status}): {event}");
    assert_eq!(event["source"], "manual");

    // A later grab at the staged instant lands on the staged event.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "readings": [{ "parameter_id": GLOBAL_PARAM_DO_ID, "value": 9.0, "time": T1 }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*) AS n FROM readings r \
                 JOIN collection_events ce ON ce.id = r.collection_event_id \
                 WHERE ce.notes = 'spring campaign' AND r.time = '{T1}'"
            ),
        )
        .await,
        1
    );

    // The unique key holds: staging the same instant twice is refused.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/collection_events",
        &json!({ "site_id": SITE1_ID, "collected_at": T1 }),
        &token,
    )
    .await;
    assert!(status >= 400, "a duplicate staging is refused: {body}");

    // Which is why staging goes through its own endpoint: it is find-or-create, so a second tool
    // entering the same visit joins it instead of colliding with the unique key.
    let (status, staged) = crate::common::post_json_parse_with_token(
        &app,
        "/api/collection_events/stage",
        &json!({ "site_id": SITE1_ID, "collected_at": T1, "notes": "ignored, the visit stands" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{staged}");
    assert_eq!(staged["id"], event["id"]);
    assert_eq!(staged["created"], false);
    assert_eq!(staged["notes"], "spring campaign");
}

/// Staging a visit nobody has entered yet creates it, stamped with the caller and `manual`.
#[tokio::test]
#[serial]
async fn staging_a_new_instant_creates_the_visit() {
    let (db, app, token) = setup().await;

    let (status, staged) = crate::common::post_json_parse_with_token(
        &app,
        "/api/collection_events/stage",
        &json!({ "site_id": SITE1_ID, "collected_at": T1 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{staged}");
    assert_eq!(staged["created"], true);
    assert_eq!(staged["source"], "manual");
    assert_eq!(
        scalar_i64(&db, "SELECT COUNT(*) AS n FROM collection_events").await,
        1
    );

    // An unknown site is refused rather than minting a visit nothing backs.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/collection_events/stage",
        &json!({ "site_id": Uuid::new_v4(), "collected_at": T1 }),
        &token,
    )
    .await;
    assert_eq!(status, 404, "{body}");
}

/// Scenario: an operator visits two stations on one field day, each at its own time, and stages the
/// day in one call.
///
/// Expected behaviour: one visit per row, in the order given, each find-or-create as the single
/// stage is; a row repeating another's station and instant refuses the day; an unknown station
/// refuses the whole day and stages nothing; a day naming no visit is refused.
#[tokio::test]
#[serial]
async fn a_field_day_is_staged_in_one_call() {
    let (db, app, token) = setup().await;
    let site2 = crate::common::SITE2_ID;
    const LATER: &str = "2025-06-02T14:00:00Z";

    let (status, staged) = crate::common::post_json_parse_with_token(
        &app,
        "/api/collection_events/stage_many",
        &json!({
            "visits": [
                { "site_id": SITE1_ID, "collected_at": T1 },
                { "site_id": site2, "collected_at": LATER },
                { "site_id": SITE1_ID, "collected_at": LATER },
            ],
            "notes": "trip",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{staged}");
    let staged = staged.as_array().expect("a list");
    assert_eq!(staged.len(), 3, "{staged:?}");
    assert_eq!(staged[0]["site_id"], SITE1_ID);
    assert_eq!(staged[1]["site_id"], site2);
    assert_eq!(staged[2]["site_id"], SITE1_ID);
    assert_ne!(
        staged[0]["collected_at"], staged[2]["collected_at"],
        "each row keeps its own time"
    );
    assert!(
        staged
            .iter()
            .all(|e| e["created"] == true && e["notes"] == "trip")
    );

    let (status, again) = crate::common::post_json_parse_with_token(
        &app,
        "/api/collection_events/stage_many",
        &json!({ "visits": [
            { "site_id": site2, "collected_at": LATER },
            { "site_id": SITE1_ID, "collected_at": T1 },
        ] }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{again}");
    let again = again.as_array().expect("a list");
    assert_eq!(
        again[0]["id"], staged[1]["id"],
        "the day joins the visits that stand"
    );
    assert_eq!(again[1]["id"], staged[0]["id"]);
    assert!(again.iter().all(|e| e["created"] == false));

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/collection_events/stage_many",
        &json!({ "visits": [
            { "site_id": SITE1_ID, "collected_at": "2025-06-03T08:00:00Z" },
            { "site_id": SITE1_ID, "collected_at": "2025-06-03T08:00:00Z" },
        ] }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("row 2 repeats row 1"), "{body}");

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/collection_events/stage_many",
        &json!({ "visits": [
            { "site_id": SITE1_ID, "collected_at": "2025-06-03T08:00:00Z" },
            { "site_id": Uuid::new_v4(), "collected_at": "2025-06-03T09:00:00Z" },
        ] }),
        &token,
    )
    .await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(
        scalar_i64(&db, "SELECT COUNT(*) AS n FROM collection_events").await,
        3,
        "a refused day stages nothing"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/collection_events/stage_many",
        &json!({ "visits": [] }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "{body}");
}

#[tokio::test]
#[serial]
async fn continuous_readings_get_no_event() {
    let (db, app, token) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &json!({
            "readings": [{
                "site_id": SITE1_ID,
                "parameter_id": GLOBAL_PARAM_TEMP_ID,
                "time": T1,
                "raw_value": 3.3,
                "measurement_type": "continuous",
            }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        event_row(&db, T1).await.is_none(),
        "a logger cadence is not a visit"
    );
}

#[tokio::test]
#[serial]
async fn an_event_with_readings_cannot_be_deleted() {
    // Scenario: a staged visit has spot readings attached; an operator deletes the event.
    // Expected behaviour: the delete is refused, and the visit still lists its readings. Deleting
    // it would detach the readings from every visit with no route to re-attach them.
    let (db, app, token) = setup().await;
    let (status, event) = crate::common::post_json_parse_with_token(
        &app,
        "/api/collection_events/stage",
        &json!({ "site_id": SITE1_ID, "collected_at": T1 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{event}");
    let event_id = event["id"].as_str().unwrap().to_string();
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "readings": [{ "parameter_id": GLOBAL_PARAM_DO_ID, "value": 9.0, "time": T1 }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    for patch in [
        json!({ "collected_at": "2025-06-01T09:00:00Z" }),
        json!({ "site_id": crate::common::SITE2_ID }),
    ] {
        let (status, body) = crate::common::put_json_with_token(
            &app,
            &format!("/api/collection_events/{event_id}"),
            &patch,
            &token,
        )
        .await;
        assert_eq!(status, 409, "a visit with readings cannot move: {body}");
    }

    let (status, body) = crate::common::delete_with_token(
        &app,
        &format!("/api/collection_events/{event_id}"),
        &token,
    )
    .await;
    assert_eq!(
        status, 409,
        "an event with readings is not deletable: {body}"
    );
    assert!(body.contains("reading"), "the refusal says why: {body}");

    let attached = scalar_i64(
        &db,
        &format!("SELECT COUNT(*) AS n FROM readings WHERE collection_event_id = '{event_id}'"),
    )
    .await;
    assert_eq!(attached, 1, "the reading is still attached");
    let (status, visits) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{SITE1_ID}/visits"), &token)
            .await;
    assert_eq!(status, 200, "{visits}");
    let listed = visits["visits"]
        .as_array()
        .or_else(|| visits["events"].as_array())
        .or_else(|| visits.as_array())
        .map(|v| {
            v.iter()
                .any(|e| e["id"] == event_id || e["event_id"] == event_id)
        })
        .unwrap_or(false);
    assert!(listed, "the visit still lists: {visits}");
}

#[tokio::test]
#[serial]
async fn an_empty_event_can_be_deleted() {
    let (_db, app, token) = setup().await;
    let (status, event) = crate::common::post_json_parse_with_token(
        &app,
        "/api/collection_events/stage",
        &json!({ "site_id": SITE1_ID, "collected_at": T1 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{event}");
    let (status, body) = crate::common::delete_with_token(
        &app,
        &format!("/api/collection_events/{}", event["id"].as_str().unwrap()),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "a visit nothing references is deletable: {body}"
    );
}

#[tokio::test]
#[serial]
async fn an_empty_event_can_move_but_not_onto_another_visit() {
    let (_db, app, token) = setup().await;
    let mut events = Vec::new();
    for time in [T1, "2025-06-01T09:00:00Z"] {
        let (status, event) = crate::common::post_json_parse_with_token(
            &app,
            "/api/collection_events/stage",
            &json!({ "site_id": SITE1_ID, "collected_at": time }),
            &token,
        )
        .await;
        assert_eq!(status, 200, "{event}");
        events.push(event["id"].as_str().unwrap().to_string());
    }
    let path = format!("/api/collection_events/{}", events[0]);
    let (status, body) = crate::common::put_json_with_token(
        &app,
        &path,
        &json!({ "collected_at": "2025-06-01T09:00:00Z" }),
        &token,
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(body.contains(&events[1]), "{body}");
    let (status, body) = crate::common::put_json_with_token(
        &app,
        &path,
        &json!({ "collected_at": "2025-06-01T10:00:00Z", "notes": "Corrected time" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

/// A stream paired to the wrong site is unpaired and paired again elsewhere. The readings must
/// follow, so the first site's visit is gone and the second site's visit holds them.
#[tokio::test]
#[serial]
async fn re_pairing_moves_the_visit_to_the_new_site() {
    let (db, app, token) = setup().await;
    let (sync_token, _service) = crate::common::seed_sync_session_token(&db).await;

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({"source_system": "metalp", "source_key": "stn:moved:reps", "measurement_type": "spot"}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "{stream}");
    let stream_id = crate::common::e2e::id_of(&stream);

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/ingest",
        &json!({
            "stream_id": stream_id,
            "collection": true,
            "readings": [
                { "time": T1, "raw_value": 1.0, "replicate_index": 0 },
                { "time": T1, "raw_value": 2.0, "replicate_index": 1 },
            ],
        }),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({"site_parameter_id": crate::common::PARAM_S1_DO_ID}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (site1_event, _) = event_row(&db, T1).await.expect("first pairing attached");

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/unpair"),
        &json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint AS n FROM readings WHERE collection_event_id = '{site1_event}'"
            ),
        )
        .await,
        0,
        "unpairing must release the readings from the first site's visit"
    );
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint AS n FROM collection_events WHERE id = '{site1_event}'"
            ),
        )
        .await,
        0,
        "the first site's visit is left with no readings and must be deleted"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({"site_parameter_id": crate::common::PARAM_S2_DO_ID}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint AS n FROM readings r \
                 JOIN collection_events ce ON ce.id = r.collection_event_id \
                 WHERE r.stream_id = '{stream_id}' \
                   AND ce.site_id = '{}' AND ce.collected_at = '{T1}'",
                crate::common::SITE2_ID
            ),
        )
        .await,
        2,
        "re-pairing must attach the readings to the second site's visit"
    );
}

/// Scenario: a field day covering two stations, entered as one block and saved in one sequence.
///
/// Expected behaviour: each station's visit is staged and saved on its own, so a save the Check
/// gate refuses at the second station leaves the first station's visit standing with its readings.
#[tokio::test]
#[serial]
async fn a_field_day_saves_each_station_independently() {
    let (db, app, token) = setup().await;
    let day = "2025-06-02T07:00:00Z";

    let mut staged = Vec::new();
    for site in [SITE1_ID, crate::common::SITE2_ID] {
        let (status, event) = crate::common::post_json_parse_with_token(
            &app,
            "/api/collection_events/stage",
            &json!({ "site_id": site, "collected_at": day }),
            &token,
        )
        .await;
        assert_eq!(status, 200, "stage {site}: {event}");
        assert_eq!(event["created"], true);
        staged.push(event["id"].as_str().unwrap().to_string());
    }
    assert_ne!(
        staged[0], staged[1],
        "a visit per station, not one for the day"
    );

    // The first station's values are screened, and saved under the check that screened them.
    let (status, checked) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/seasonal_check",
        &json!({
            "site_id": SITE1_ID,
            "time": day,
            "values": [{ "parameter_id": GLOBAL_PARAM_DO_ID, "value": 9.4 }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "check the first station: {checked}");
    let check_id = checked["check_id"].as_str().unwrap().to_string();

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "check_id": check_id,
            "readings": [{ "parameter_id": GLOBAL_PARAM_DO_ID, "value": 9.4, "time": day }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "the screened station saves: {body}");

    // The second station is sent under the first station's check, which screened neither its
    // values nor its site: the save is refused rather than admitted on someone else's screening.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": crate::common::SITE2_ID,
            "check_id": check_id,
            "readings": [{ "parameter_id": GLOBAL_PARAM_DO_ID, "value": 11.8, "time": day }],
        }),
        &token,
    )
    .await;
    assert!(
        status >= 400,
        "an unscreened station is refused: {status} {body}"
    );

    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*) AS n FROM readings r \
                 JOIN collection_events ce ON ce.id = r.collection_event_id \
                 WHERE ce.id = '{}' AND r.time = '{day}'",
                staged[0]
            ),
        )
        .await,
        1,
        "the station that saved keeps its reading"
    );
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*) AS n FROM readings WHERE collection_event_id = '{}'",
                staged[1]
            ),
        )
        .await,
        0,
        "the refused station stores nothing"
    );
    assert_eq!(
        scalar_i64(
            &db,
            &format!("SELECT COUNT(*) AS n FROM collection_events WHERE collected_at = '{day}'"),
        )
        .await,
        2,
        "both visits stand: a refused save does not unstage its visit"
    );
}

/// M87: the portal's Delete row. A visit is retracted as one selection, and the set that
/// retracted it re-asserts exactly what it withdrew.
#[tokio::test]
#[serial]
async fn a_visit_is_withdrawn_and_re_asserted_as_one_set() {
    let (db, app, token) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "readings": [
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 10.0, "time": T1 },
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 12.0, "time": T1 },
                { "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 4.2, "time": T1 },
            ],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (event_id, _) = event_row(&db, T1).await.expect("the visit");

    let withdrawn = |db: DatabaseConnection, event: String| async move {
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*) AS n FROM readings \
                 WHERE collection_event_id = '{event}' AND withdrawn_at IS NOT NULL"
            ),
        )
        .await
    };
    assert_eq!(withdrawn(db.clone(), event_id.clone()).await, 0);

    let selection = json!({ "collection_event_id": event_id });
    let decision = json!({ "kind": "withdraw", "reason": "contaminated vials" });
    let (status, preview) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/edits/preview",
        &json!({ "selection": selection, "decision": decision }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{preview}");
    assert_eq!(
        preview["rows"].as_array().unwrap().len(),
        3,
        "every replicate of every parameter at the visit: {preview}"
    );
    let preview_id = preview["preview_id"].as_str().unwrap().to_string();

    let (status, committed) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/edits",
        &json!({ "selection": selection, "decision": decision, "preview_id": preview_id }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{committed}");
    assert_eq!(committed["rows_decided"], 3);
    assert_eq!(withdrawn(db.clone(), event_id.clone()).await, 3);
    assert!(
        event_row(&db, T1).await.is_some(),
        "the visit itself still stands; only its measurements are retracted"
    );

    let set_id = committed["set_id"]
        .as_str()
        .expect("a visit withdrawal is one set")
        .to_string();
    let (status, rolled) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/readings/edits/sets/{set_id}/rollback"),
        &json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{rolled}");
    assert_eq!(rolled["rolled_back"], 3);
    assert_eq!(
        withdrawn(db.clone(), event_id.clone()).await,
        0,
        "the rollback re-asserts exactly what the set withdrew"
    );

    let (status, again) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/readings/edits/sets/{set_id}/rollback"),
        &json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 409, "a set is rolled back once: {again}");
}

async fn constant_id(db: &DatabaseConnection, name: &str) -> String {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO constants (id, name, value, units, description) \
             VALUES (gen_random_uuid(), '{name}', 0.209446, 'mol/mol', 'oxygen mole fraction') \
             ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value"
        ),
    )
    .await;
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT id FROM constants WHERE name = '{name}'"),
    ))
    .await
    .unwrap()
    .expect("the constant is there")
    .try_get::<Uuid>("", "id")
    .unwrap()
    .to_string()
}

async fn queued_of_kind(db: &DatabaseConnection, kind: &str) -> Vec<serde_json::Value> {
    db.query_all_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT params FROM reprocessing_jobs WHERE trigger_type = '{kind}' ORDER BY created_at"
        ),
    ))
    .await
    .unwrap()
    .iter()
    .map(|r| r.try_get::<serde_json::Value>("", "params").unwrap())
    .collect()
}

/// Scenario: an administrator corrects a constant's value, so every stored output computed from
/// the old one states a number nothing supports any more.
///
/// Expected behaviour: the save enqueues the recompute scoped to that constant, which is the
/// migrate arm Q170 chose: a constant carries no versions, so there is no "new measurements only"
/// side to offer. The job names the value on each side, which is what makes the ledger rows the
/// recompute writes readable back to this edit. A units or description edit changes no calculation
/// and recomputes nothing.
#[tokio::test]
#[serial]
async fn a_constant_value_edit_queues_one_recompute_naming_it() {
    let (db, app, token) = setup().await;
    let id = constant_id(&db, "xO2").await;

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/constants/{id}"),
        &json!({ "description": "oxygen mole fraction in dry air" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        queued_of_kind(&db, "event_recompute").await.is_empty(),
        "a description edit changes no calculation"
    );

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/constants/{id}"),
        &json!({ "value": 0.2095 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let queued = queued_of_kind(&db, "event_recompute").await;
    assert_eq!(queued.len(), 1, "one recompute for the change: {queued:?}");
    assert_eq!(queued[0]["constant"], "xO2");
    assert_eq!(queued[0]["previous_value"], 0.209446);
    assert_eq!(queued[0]["value"], 0.2095);
    assert!(
        queued_of_kind(&db, "event_audit").await.is_empty(),
        "the recompute repairs what the audit would only have reported"
    );

    // The key coalesces only while the run is still waiting: a claim releases it, because a change
    // landing mid-run needs a run of its own. The worker pool is live here, so the pending state is
    // restored rather than raced for.
    crate::common::exec(
        &db,
        "UPDATE reprocessing_jobs SET status = 'queued', lease_expires_at = NULL, \
         dedupe_key = 'event_recompute:constant:xO2' WHERE trigger_type = 'event_recompute'",
    )
    .await;

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/constants/{id}"),
        &json!({ "value": 0.2096 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        queued_of_kind(&db, "event_recompute").await.len(),
        1,
        "the pending recompute already covers this constant"
    );
}

/// Scenario: a field day's block is pasted into the batch grid and saved. Each station is staged
/// and written on its own, so a station the API refuses reports why and the stations already
/// written stay written (M51).
///
/// Expected behaviour: the first station's visit holds its reading; the second is refused naming
/// the parameter its site does not configure, and nothing of it is stored.
#[tokio::test]
#[serial]
async fn a_refused_station_leaves_the_stations_already_saved_alone() {
    let (db, app, token) = setup().await;
    let site2 = crate::common::SITE2_ID;

    for site in [SITE1_ID, site2] {
        let (status, body) = crate::common::post_json_with_token(
            &app,
            "/api/collection_events/stage",
            &json!({ "site_id": site, "collected_at": T1 }),
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
    }

    let save = |site: &'static str| {
        let app = app.clone();
        let token = token.clone();
        async move {
            crate::common::post_json_with_token(
                &app,
                "/api/grab_samples",
                &json!({
                    "site_id": site,
                    "mode": "replace",
                    "readings": [{
                        "parameter_id": crate::common::GLOBAL_PARAM_DEPTH_ID,
                        "value": 1.5,
                        "time": T1,
                        "replicate_index": 0,
                    }],
                }),
                &token,
            )
            .await
        }
    };

    let (status, body) = save(SITE1_ID).await;
    assert_eq!(status, 200, "{body}");

    // The downstream station carries no Depth slot, so its row of the block is refused.
    let (status, body) = save(site2).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.contains("not configured for site"),
        "the refusal names what the site does not carry: {body}"
    );

    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*) AS n FROM readings \
                 WHERE site_id = '{SITE1_ID}' AND parameter_id = '{depth}' AND time = '{T1}'",
                depth = crate::common::GLOBAL_PARAM_DEPTH_ID,
            ),
        )
        .await,
        1,
        "the station saved before the refusal keeps its reading"
    );
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*) AS n FROM readings \
                 WHERE site_id = '{site2}' AND parameter_id = '{depth}' AND time = '{T1}'",
                depth = crate::common::GLOBAL_PARAM_DEPTH_ID,
            ),
        )
        .await,
        0,
        "the refused station stored nothing"
    );
}

/// Scenario: two technicians finish the same station and press Save in the same instant, so the
/// second staging is already waiting on the first when it commits.
///
/// Expected behaviour: one of them creates the visit and the other is handed the visit that
/// stands, rather than a refusal against a row it never attempted.
#[tokio::test]
#[serial]
async fn a_visit_staged_twice_at_once_lands_on_one_row() {
    let (db, app, token) = setup().await;

    // The first technician's insert, committed only once the second is waiting on it.
    let first = db.begin().await.expect("a transaction");
    first
        .execute_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "INSERT INTO collection_events (site_id, collected_at, source, created_by) \
                 VALUES ('{SITE1_ID}', '{T1}', 'manual', 'first@example.org')"
            ),
        ))
        .await
        .expect("the first technician's visit");

    let second = tokio::spawn({
        let app = app.clone();
        let token = token.clone();
        async move {
            crate::common::post_json_parse_with_token(
                &app,
                "/api/collection_events/stage",
                &json!({ "site_id": SITE1_ID, "collected_at": T1 }),
                &token,
            )
            .await
        }
    });
    assert!(
        staging_blocked(&db).await,
        "the second staging never reached the conflicting insert, so the race was not run"
    );
    first.commit().await.expect("the first technician commits");

    let (status, staged) = second.await.expect("the second staging returns");
    assert_eq!(status, 200, "{staged}");
    assert_eq!(
        staged["created"], false,
        "the second stager is handed the visit that stands: {staged}"
    );
    assert_eq!(staged["created_by"], "first@example.org");
    assert_eq!(
        scalar_i64(&db, "SELECT COUNT(*) AS n FROM collection_events").await,
        1
    );
}

/// The moment the race exists: the second staging's insert is waiting on the lock the first holds.
async fn staging_blocked(db: &DatabaseConnection) -> bool {
    for _ in 0..100 {
        let waiting = scalar_i64(
            db,
            "SELECT COUNT(*) AS n FROM pg_stat_activity \
              WHERE state = 'active' AND wait_event_type = 'Lock' \
                AND query LIKE '%INSERT INTO collection_events%'",
        )
        .await;
        if waiting > 0 {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

/// A visit's id and its verification state: pending a ruling, and withdrawn.
async fn visit_state(db: &DatabaseConnection, time: &str) -> (String, bool, bool) {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT id::text AS id, unverified, withdrawn_at IS NOT NULL AS withdrawn \
               FROM collection_events \
              WHERE site_id = '{SITE1_ID}' AND collected_at = '{time}'"
        ),
    ))
    .await
    .unwrap()
    .map(|r| {
        (
            r.try_get::<String>("", "id").unwrap(),
            r.try_get::<bool>("", "unverified").unwrap(),
            r.try_get::<bool>("", "withdrawn").unwrap(),
        )
    })
    .expect("the visit stands")
}

/// Stamp a visit pending and file the hold a manager rules on, as an intern's staging would.
async fn hold_the_visit_open(db: &DatabaseConnection, event_id: &str, time: &str) -> String {
    let hold = Uuid::new_v4().to_string();
    crate::common::exec(
        db,
        &format!("UPDATE collection_events SET unverified = TRUE WHERE id = '{event_id}'"),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO replicate_audit_holds \
                 (id, stream_id, site_id, parameter_id, group_time, kind, expected, computed, \
                  delta, status) \
             VALUES ('{hold}', NULL, '{SITE1_ID}', NULL, '{time}', 'unverified_visit', \
                     '{{\"state\": \"verified\"}}'::jsonb, \
                     '{{\"state\": \"unverified\"}}'::jsonb, '{{}}'::jsonb, 'pending')"
        ),
    )
    .await;
    hold
}

async fn resolve_hold(app: &axum::Router, hold: &str, mode: &str, token: &str) -> (u16, String) {
    crate::common::post_json_with_token(
        app,
        &format!("/api/sync/replicate_audit_holds/{hold}/resolve"),
        &json!({ "mode": mode }),
        token,
    )
    .await
}

/// The visit's own verification state (Q177). A field day an intern opens is pending until a
/// manager rules on it, and until then its measurements cannot be verified either: what is in
/// question is whether the visit happened at all.
///
/// The stager's level is a unit decision (`visit_lands_pending`); what only real SQL shows is that
/// a verified stager raises no hold, that a rejection withdraws the readings with the visit, and
/// that the entry ruling is refused while the visit stands unruled.
#[tokio::test]
#[serial]
async fn a_verified_stager_opens_a_verified_field_day() {
    let (db, app, token) = setup().await;

    let (status, staged) = crate::common::post_json_parse_with_token(
        &app,
        "/api/collection_events/stage",
        &json!({ "site_id": SITE1_ID, "collected_at": T1 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{staged}");
    assert_eq!(staged["unverified"], false);
    let (_, unverified, withdrawn) = visit_state(&db, T1).await;
    assert!(
        !unverified,
        "a full-permission caller's visit needs no ruling"
    );
    assert!(!withdrawn);
    assert_eq!(
        scalar_i64(
            &db,
            "SELECT COUNT(*) AS n FROM replicate_audit_holds WHERE kind = 'unverified_visit'"
        )
        .await,
        0,
        "no ruling is owed, so the review queue carries nothing"
    );
}

#[tokio::test]
#[serial]
async fn verifying_a_measurement_is_refused_while_its_field_day_is_unruled() {
    let (db, app, token) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "readings": [{ "parameter_id": GLOBAL_PARAM_DO_ID, "value": 9.0, "time": T1 }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (event_id, _, _) = visit_state(&db, T1).await;
    let visit_hold = hold_the_visit_open(&db, &event_id, T1).await;

    let entry_hold = Uuid::new_v4().to_string();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO replicate_audit_holds \
                 (id, stream_id, site_id, parameter_id, group_time, kind, expected, computed, \
                  delta, status) \
             VALUES ('{entry_hold}', NULL, '{SITE1_ID}', '{GLOBAL_PARAM_DO_ID}', '{T1}', \
                     'unverified_entry', '{{\"state\": \"verified\"}}'::jsonb, \
                     '{{\"state\": \"unverified\"}}'::jsonb, '{{}}'::jsonb, 'pending')"
        ),
    )
    .await;

    let (status, body) = resolve_hold(&app, &entry_hold, "verify", &token).await;
    assert_eq!(status, 409, "the field day is ruled on first: {body}");
    assert!(
        body.contains(&event_id),
        "the refusal names the visit: {body}"
    );

    // Verifying the field day releases the measurement's ruling, and verifies none of them: each
    // value is its own entry with its own hold.
    let (status, body) = resolve_hold(&app, &visit_hold, "verify", &token).await;
    assert_eq!(status, 200, "{body}");
    let (_, unverified, withdrawn) = visit_state(&db, T1).await;
    assert!(!unverified, "the visit is accepted");
    assert!(!withdrawn);
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*) AS n FROM replicate_audit_holds \
                  WHERE id = '{entry_hold}' AND status = 'pending'"
            )
        )
        .await,
        1,
        "the measurement is still owed a ruling of its own"
    );
    let (status, body) = resolve_hold(&app, &entry_hold, "verify", &token).await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
#[serial]
async fn rejecting_a_field_day_withdraws_it_with_its_readings() {
    let (db, app, token) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "readings": [
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 9.0, "time": T1 },
                { "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 4.2, "time": T1 },
            ],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (event_id, _, _) = visit_state(&db, T1).await;
    let hold = hold_the_visit_open(&db, &event_id, T1).await;

    let (status, body) = resolve_hold(&app, &hold, "reject", &token).await;
    assert_eq!(status, 200, "{body}");
    let (_, unverified, withdrawn) = visit_state(&db, T1).await;
    assert!(withdrawn, "the visit carries the rejection");
    assert!(
        !unverified,
        "a rejected visit is no longer awaiting a ruling"
    );
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*) AS n FROM readings \
                  WHERE collection_event_id = '{event_id}' AND withdrawn_at IS NULL"
            )
        )
        .await,
        0,
        "every reading entered at the visit is withdrawn beside it"
    );
    // Nothing deletes: the readings and the visit are both still there to be re-asserted.
    assert_eq!(
        scalar_i64(
            &db,
            &format!("SELECT COUNT(*) AS n FROM readings WHERE collection_event_id = '{event_id}'")
        )
        .await,
        2
    );
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*) AS n FROM reading_decisions WHERE kind = 'reject' \
                       AND time = '{T1}'"
            )
        )
        .await,
        2,
        "each withdrawal is on the curation record"
    );
}
