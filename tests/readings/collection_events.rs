//! Collection events (D7): every attributed spot instant belongs to one
//! `(site, collected_at)` event, whatever path wrote it. A grab entry attaches a manual event, a
//! sync-service ingest a portal_sync one, a pairing backfill attaches late, and the staging
//! endpoint is the portal's New Entry.
//!
//! Run: cargo test --test readings collection_events -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use uuid::Uuid;
use serial_test::serial;

use crate::common::{GLOBAL_PARAM_DO_ID, GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const T1: &str = "2025-06-01T08:00:00Z";

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one(Statement::from_string(
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
    db.query_one(Statement::from_string(
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

    let (event_id, source) = event_row(&db, T1).await.expect("the portal row became a visit");
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

    let (_, source) = event_row(&db, T1).await.expect("pairing attached the visit");
    assert_eq!(source, "portal_sync", "a portal stream's backfill is a synced visit");
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
