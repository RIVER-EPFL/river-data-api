//! No measurement without an instrument.
//!
//! A reading whose instrument is unknown has lost its provenance and nothing recovers it later, so
//! the rule is held by the database rather than by each writer: `readings_inherit_stream_instrument`
//! fills the column from the row's own stream when a writer names none, and
//! `readings_instrument_required` refuses what is left. A derived value carries the slot but no
//! instrument and is the one exemption.
//!
//! Run: cargo test --test readings instrument_required -- --test-threads=1

use sea_orm::{ConnectionTrait, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const AT: &str = "2025-07-02T09:00:00Z";

async fn setup() -> (axum::Router, sea_orm::DatabaseConnection, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    (crate::common::build_test_app(db.clone()), db, token)
}

async fn insert_reading(
    db: &sea_orm::DatabaseConnection,
    stream_id: Uuid,
    measurement_type: &str,
) -> Result<(), sea_orm::DbErr> {
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO readings (stream_id, time, replicate_index, raw_value, measurement_type) \
         VALUES ($1, $2::timestamptz, 0, 1.0, $3)",
        [stream_id.into(), AT.into(), measurement_type.into()],
    ))
    .await
    .map(|_| ())
}

async fn insert_untyped_reading(
    db: &sea_orm::DatabaseConnection,
    stream_id: Uuid,
) -> Result<(), sea_orm::DbErr> {
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO readings (stream_id, time, replicate_index, raw_value) \
         VALUES ($1, $2::timestamptz, 0, 1.0)",
        [stream_id.into(), AT.into()],
    ))
    .await
    .map(|_| ())
}

async fn create_stream(
    db: &sea_orm::DatabaseConnection,
    sensor_id: Option<Uuid>,
) -> Uuid {
    let id = Uuid::new_v4();
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO data_streams (id, source_system, source_key, is_active, sensor_id) \
         VALUES ($1, 'instrument-required', $2, true, $3)",
        [id.into(), id.to_string().into(), sensor_id.into()],
    ))
    .await
    .expect("create stream");
    id
}

async fn sensor_of(db: &sea_orm::DatabaseConnection, stream_id: Uuid) -> Option<Uuid> {
    db.query_one_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT sensor_id FROM readings WHERE stream_id = $1",
        [stream_id.into()],
    ))
    .await
    .expect("query")
    .and_then(|row| row.try_get::<Option<Uuid>>("", "sensor_id").ok().flatten())
}

/// Expected behaviour: a writer that names no instrument stores the stream's, not NULL.
#[tokio::test]
#[serial]
async fn a_reading_naming_no_instrument_inherits_its_stream_s() {
    let (_app, db, _token) = setup().await;
    let sensor_id = Uuid::parse_str(crate::common::db::FIXTURE_SENSOR_ID).expect("fixture uuid");
    let stream_id = create_stream(&db, Some(sensor_id)).await;

    insert_reading(&db, stream_id, "continuous")
        .await
        .expect("a reading on a stream with an instrument is stored");
    assert_eq!(
        sensor_of(&db, stream_id).await,
        Some(sensor_id),
        "the stream's instrument is stamped on the reading"
    );
}

/// Expected behaviour: nothing can store a measurement whose instrument is unknown. A derived value
/// is the exemption: it carries the slot it was computed for and no instrument measured it.
#[tokio::test]
#[serial]
async fn a_reading_with_no_instrument_anywhere_is_refused_unless_derived() {
    let (_app, db, _token) = setup().await;
    let stream_id = create_stream(&db, None).await;

    let refused = insert_reading(&db, stream_id, "spot").await;
    assert!(
        refused
            .as_ref()
            .err()
            .is_some_and(|e| e.to_string().contains("readings_instrument_required")),
        "an instrument-less reading is refused: {refused:?}"
    );

    insert_reading(&db, stream_id, "derived")
        .await
        .expect("a derived value carries no instrument and is stored");
}

/// Expected behaviour: the grab entry path attributes what it writes, so a hand-entered value names
/// an instrument even when the operator picked none.
#[tokio::test]
#[serial]
async fn a_grab_save_naming_no_instrument_is_attributed_to_the_slot_s_entry_instrument() {
    let (app, db, token) = setup().await;

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "readings": [{
                "parameter_id": GLOBAL_PARAM_TEMP_ID,
                "time": AT,
                "value": 12.5,
            }],
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "grab save ({status}): {body}");

    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT s.name FROM readings r JOIN sensors s ON s.id = r.sensor_id \
             WHERE r.site_id = $1::uuid AND r.time = $2::timestamptz",
            [SITE1_ID.into(), AT.into()],
        ))
        .await
        .expect("query")
        .expect("the grab reading names an instrument");
    let name: String = row.try_get("", "name").expect("instrument name");
    assert!(
        name.ends_with("(grab entry)"),
        "the channel's own instrument, not the deployed probe: {name}"
    );
}

/// Expected behaviour: an absent `measurement_type` reads as continuous everywhere else, so it
/// claims no exemption here either. The rule must not go quiet on the column's ordinary state.
#[tokio::test]
#[serial]
async fn a_reading_with_no_instrument_and_no_classification_is_refused() {
    let (_app, db, _token) = setup().await;
    let stream_id = create_stream(&db, None).await;

    let refused = insert_untyped_reading(&db, stream_id).await;
    assert!(
        refused
            .as_ref()
            .err()
            .is_some_and(|e| e.to_string().contains("readings_instrument_required")),
        "an unclassified instrument-less reading is refused: {refused:?}"
    );
}
