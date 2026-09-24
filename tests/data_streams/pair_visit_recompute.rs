//! Scenario: pairing a stream (by hand, or an entry channel once its slot exists) attributes
//! readings that sit at a manual visit a calculation reads, and unpairing it takes them away again,
//! so either owes the visit an `event_recompute`.
//!
//! Expected behaviour: the recompute is queued with the pairing or the release it follows, so a
//! route that cannot queue it changes nothing and says so, and a route that answers 200 left the
//! recompute queued.
//!
//! Run: cargo test --test data_streams pair_visit_recompute -- --test-threads=1

use river_db::routes::private::data_streams::flows::pair_entry_channel;
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::sensor_lifecycle::{create_paired_stream, create_unpaired_stream_with_device};
use crate::common::{GLOBAL_PARAM_TEMP_ID, PARAM_S1_TEMP_ID, SITE1_ID, e2e, exec};

const AT: &str = "2025-10-06T09:00:00Z";

async fn setup() -> (axum::Router, String, sea_orm::DatabaseConnection) {
    let f = crate::common::seeded_app().await;
    crate::common::seed_visit_calculation(&f.db, "pair_visit_input", "DO_Temperature").await;
    (f.app, f.token, f.db)
}

/// A manual visit at `AT` holding one spot temperature reading of `stream`, attributed to the slot
/// or not as `attributed` says.
async fn visit_reading(db: &sea_orm::DatabaseConnection, stream: Uuid, attributed: bool) -> Uuid {
    let event = Uuid::new_v4();
    let (site, parameter) = if attributed {
        (format!("'{SITE1_ID}'"), format!("'{GLOBAL_PARAM_TEMP_ID}'"))
    } else {
        ("NULL".to_string(), "NULL".to_string())
    };
    exec(
        db,
        &format!(
            "INSERT INTO collection_events (id, site_id, collected_at, source) \
             VALUES ('{event}', '{SITE1_ID}', '{AT}', 'manual')"
        ),
    )
    .await;
    exec(
        db,
        &format!(
            "INSERT INTO readings \
                 (stream_id, site_id, parameter_id, time, raw_value, replicate_index, \
                  measurement_type, collection_event_id) \
             VALUES ('{stream}', {site}, {parameter}, '{AT}', 10, 0, 'spot', '{event}')"
        ),
    )
    .await;
    event
}

async fn pair(app: &axum::Router, token: &str, stream: Uuid) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        app,
        &format!("/api/streams/{stream}/pair"),
        &json!({ "site_parameter_id": PARAM_S1_TEMP_ID }),
        token,
    )
    .await
}

async fn unpair(app: &axum::Router, token: &str, stream: Uuid) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        app,
        &format!("/api/streams/{stream}/unpair"),
        &json!({}),
        token,
    )
    .await
}

async fn recomputes_queued(db: &sea_orm::DatabaseConnection, event: Uuid) -> i64 {
    e2e::count(
        db,
        &format!(
            "SELECT COUNT(*)::bigint FROM reprocessing_jobs \
             WHERE trigger_type = 'event_recompute' AND trigger_id = '{event}'"
        ),
    )
    .await
}

async fn paired(db: &sea_orm::DatabaseConnection, stream: Uuid) -> i64 {
    e2e::count(
        db,
        &format!(
            "SELECT COUNT(*)::bigint FROM data_streams \
             WHERE id = '{stream}' AND site_parameter_id IS NOT NULL"
        ),
    )
    .await
}

async fn attributed(db: &sea_orm::DatabaseConnection, stream: Uuid) -> i64 {
    e2e::count(
        db,
        &format!(
            "SELECT COUNT(*)::bigint FROM readings \
             WHERE stream_id = '{stream}' AND site_id IS NOT NULL"
        ),
    )
    .await
}

#[tokio::test]
#[serial]
async fn a_pairing_queues_the_recompute_of_the_visits_it_attributes() {
    let (app, token, db) = setup().await;
    let stream = create_unpaired_stream_with_device(&db, "visit-pair", "SB1-VISIT-PAIR").await;
    let event = visit_reading(&db, stream, false).await;

    let (status, body) = pair(&app, &token, stream).await;

    assert_eq!(status, 200, "pair: {body}");
    assert_eq!(recomputes_queued(&db, event).await, 1);
}

#[tokio::test]
#[serial]
async fn a_pairing_whose_visit_recompute_cannot_be_queued_pairs_nothing() {
    let (app, token, db) = setup().await;
    let stream = create_unpaired_stream_with_device(&db, "visit-pair-lost", "SB1-VISIT-LOST").await;
    visit_reading(&db, stream, false).await;

    crate::common::jobs::refuse_enqueue(&db, "event_recompute").await;
    let (status, body) = pair(&app, &token, stream).await;
    crate::common::jobs::restore_enqueue(&db).await;

    assert_eq!(status, 500, "the pairing reports the failure: {body}");
    assert_eq!(paired(&db, stream).await, 0, "the stream is still unpaired");
    assert_eq!(
        attributed(&db, stream).await,
        0,
        "no reading was attributed"
    );
    let (status, body) = pair(&app, &token, stream).await;
    assert_eq!(status, 200, "a retry pairs the stream: {body}");
}

#[tokio::test]
#[serial]
async fn an_unpairing_whose_visit_recompute_cannot_be_queued_unpairs_nothing() {
    let (app, token, db) = setup().await;
    let stream = create_paired_stream(&db, "visit-unpair-lost", PARAM_S1_TEMP_ID).await;
    let event = visit_reading(&db, stream, true).await;

    crate::common::jobs::refuse_enqueue(&db, "event_recompute").await;
    let (status, body) = unpair(&app, &token, stream).await;
    crate::common::jobs::restore_enqueue(&db).await;

    assert_eq!(status, 500, "the unpairing reports the failure: {body}");
    assert_eq!(paired(&db, stream).await, 1, "the stream is still paired");
    assert_eq!(
        attributed(&db, stream).await,
        1,
        "its reading is still attributed"
    );
    let (status, body) = unpair(&app, &token, stream).await;
    assert_eq!(status, 200, "a retry unpairs the stream: {body}");
    assert_eq!(recomputes_queued(&db, event).await, 1);
}

#[tokio::test]
#[serial]
async fn an_entry_channel_pairing_whose_visit_recompute_cannot_be_queued_pairs_nothing() {
    let (_app, _token, db) = setup().await;
    let stream = create_unpaired_stream_with_device(&db, "visit-entry", "SB1-VISIT-ENTRY").await;
    let event = visit_reading(&db, stream, false).await;
    let slot = Some(PARAM_S1_TEMP_ID.parse().unwrap());

    crate::common::jobs::refuse_enqueue(&db, "event_recompute").await;
    let refused = pair_entry_channel(&db, stream, slot, "test").await;
    crate::common::jobs::restore_enqueue(&db).await;

    assert!(refused.is_err(), "the pairing reports the refused enqueue");
    assert_eq!(
        paired(&db, stream).await,
        0,
        "the channel is still unpaired"
    );
    assert_eq!(
        attributed(&db, stream).await,
        0,
        "no reading was attributed"
    );
    pair_entry_channel(&db, stream, slot, "test")
        .await
        .expect("a retry pairs the channel");
    assert_eq!(paired(&db, stream).await, 1);
    assert_eq!(recomputes_queued(&db, event).await, 1);
}

/// Scenario: a visit whose only reading came through the stream carries a note, and the stream is
/// unpaired.
///
/// Expected behaviour: the visit and its note stay (CID10), with no reading on it.
#[tokio::test]
#[serial]
async fn an_unpairing_keeps_the_visit_it_empties() {
    let (app, token, db) = setup().await;
    let stream = create_paired_stream(&db, "visit-unpair-empties", PARAM_S1_TEMP_ID).await;
    let event = visit_reading(&db, stream, true).await;
    exec(
        &db,
        &format!("UPDATE collection_events SET notes = 'turbid' WHERE id = '{event}'"),
    )
    .await;

    let (status, body) = unpair(&app, &token, stream).await;

    assert_eq!(status, 200, "unpair: {body}");
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint FROM collection_events \
                 WHERE id = '{event}' AND notes = 'turbid'"
            ),
        )
        .await,
        1,
        "the emptied visit keeps its note"
    );
}
