//! Scenario: readings were pinned to an instrument or a curve while the pin surfaces existed, and
//! those surfaces have since been retired, leaving nothing that could clear a pin.
//!
//! Expected behaviour: every live pin is inverted the way the route did it, the reading's column
//! goes back to what it held before, both halves stay readable in the ledger, and the slot is
//! queued for the reprocess that re-derives it. A pin already rolled back is left alone.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use crate::common::{
    GLOBAL_PARAM_TEMP_ID, SITE1_ID, cleanup_test_db, seed_test_data, setup_test_db,
};

const AT: &str = "2025-06-15T10:00:00Z";

async fn run_migration(db: &DatabaseConnection) {
    db.execute_unprepared(&migration::m20260910_000026_roll_back_live_pins::roll_back_live_pins())
        .await
        .expect("the rollback applies");
}

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

async fn stored_sensor(db: &DatabaseConnection, stream: Uuid) -> Option<Uuid> {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT sensor_id FROM readings WHERE stream_id = '{stream}' AND time = '{AT}'"),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get("", "sensor_id")
    .unwrap()
}

/// One reading pinned to a second instrument, in the shape the retired route left it: the pin
/// decision names the sensor the row held before, and the row holds the pinned one.
async fn seed_pinned_reading(db: &DatabaseConnection) -> (Uuid, Uuid, Uuid) {
    let stream = crate::common::sensor_lifecycle::create_paired_stream(
        db,
        "pinned-temp",
        crate::common::PARAM_S1_TEMP_ID,
    )
    .await;
    let owner: Uuid = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT sensor_id AS id FROM data_streams WHERE id = '{stream}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "id")
        .unwrap();
    let spare =
        crate::common::sensor_lifecycle::create_sensor(db, "Spare-probe-01", GLOBAL_PARAM_TEMP_ID)
            .await
            .id;

    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, sensor_id, time, raw_value, \
             replicate_index, measurement_type) \
             VALUES ('{stream}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{spare}', '{AT}', 4.0, 0, \
                     'continuous')"
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO reading_decisions \
                 (stream_id, time, replicate_index, kind, old, new, actor, origin) \
             VALUES ('{stream}', '{AT}', 0, 'instrument_pin', \
                     jsonb_build_object('sensor_id', '{owner}'::text), \
                     jsonb_build_object('sensor_id', '{spare}'::text), 'someone', 'manual')"
        ),
    )
    .await;
    (stream, owner, spare)
}

#[tokio::test]
#[serial]
async fn a_live_pin_is_inverted_and_its_slot_queued_for_reprocess() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_test_data(&db).await;
    let (stream, owner, spare) = seed_pinned_reading(&db).await;
    assert_eq!(
        stored_sensor(&db, stream).await,
        Some(spare),
        "the row holds the pinned instrument before the migration"
    );

    run_migration(&db).await;

    assert_eq!(
        stored_sensor(&db, stream).await,
        Some(owner),
        "the instrument the pin displaced is restored"
    );
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT count(*)::bigint AS n FROM reading_decisions \
                  WHERE stream_id = '{stream}' AND kind = 'instrument_pin' \
                    AND rolled_back_by IS NOT NULL"
            )
        )
        .await,
        1,
        "the pin is stamped with what inverted it, and is still readable"
    );
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT count(*)::bigint AS n FROM reading_decisions \
                  WHERE stream_id = '{stream}' AND kind = 'rollback' AND origin = 'rollback'"
            )
        )
        .await,
        1,
        "the inversion is itself a decision"
    );
    assert_eq!(
        scalar_i64(
            &db,
            "SELECT count(*)::bigint AS n FROM reprocessing_jobs \
              WHERE trigger_type = 'attribution_pin' AND status = 'queued'"
        )
        .await,
        1,
        "the slot is queued to re-derive from the deployment history"
    );

    // Rerunnable: a second pass finds no live pin, so it writes nothing and queues nothing twice.
    run_migration(&db).await;
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT count(*)::bigint AS n FROM reading_decisions \
                  WHERE stream_id = '{stream}' AND kind = 'rollback'"
            )
        )
        .await,
        1,
        "an already inverted pin is not inverted again"
    );
    assert_eq!(
        scalar_i64(
            &db,
            "SELECT count(*)::bigint AS n FROM reprocessing_jobs WHERE trigger_type = 'attribution_pin'"
        )
        .await,
        1,
        "and the slot is queued once"
    );
}
