//! Scenario: a database holds readings that arrived on streams no plan had paired, each stamped
//! with the instrument registration minted for its channel.
//!
//! Expected behaviour: the migration unstamps exactly those rows, leaves every attributed row and
//! every derived row alone, and leaves a CHECK that admits an unattributed row naming no
//! instrument while still refusing an attributed one.
//!
//! Run: cargo test --test migrations stage_unpaired_readings -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use crate::common::{
    cleanup_test_db, seed_test_data, setup_test_db, FIXTURE_SENSOR_ID, PARAM_S1_TEMP_ID, SITE1_ID,
};

const UNPAIRED: &str = "00000000-0000-4000-c000-0000000b2231";
const PAIRED: &str = "00000000-0000-4000-c000-0000000b2232";
const AT: &str = "2025-05-01T00:00:00Z";

/// Undo the rule the baseline creates with the table, leaving the shape a database that predates
/// this migration has: the instrument required of every non-derived row, and the trigger filling
/// it from the stream whether or not the stream is paired.
async fn revert_to_pre_staging_shape(db: &DatabaseConnection) {
    for sql in [
        "ALTER TABLE readings DROP CONSTRAINT IF EXISTS readings_instrument_required",
        "ALTER TABLE readings ADD CONSTRAINT readings_instrument_required \
         CHECK (sensor_id IS NOT NULL OR measurement_type IS NOT DISTINCT FROM 'derived')",
        "CREATE OR REPLACE FUNCTION public.readings_inherit_stream_instrument() RETURNS trigger \
           LANGUAGE plpgsql AS $$ BEGIN \
             IF NEW.sensor_id IS NULL AND NEW.measurement_type IS DISTINCT FROM 'derived' THEN \
               SELECT sensor_id INTO NEW.sensor_id FROM data_streams WHERE id = NEW.stream_id; \
             END IF; RETURN NEW; END; $$",
    ] {
        crate::common::exec(db, sql).await;
    }
}

async fn stored(db: &DatabaseConnection, stream: &str) -> (Option<String>, Option<String>) {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT sensor_id::text AS sensor_id, calibration_id::text AS calibration_id \
                   FROM readings WHERE stream_id = '{stream}' AND time = '{AT}'"
            ),
        ))
        .await
        .expect("query readings")
        .expect("the reading is stored");
    (
        row.try_get::<Option<String>>("", "sensor_id").unwrap(),
        row.try_get::<Option<String>>("", "calibration_id").unwrap(),
    )
}

#[tokio::test]
#[serial]
async fn the_unpaired_rows_are_unstamped_and_the_attributed_ones_are_left_alone() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_test_data(&db).await;
    revert_to_pre_staging_shape(&db).await;

    for (id, key, slot) in [
        (UNPAIRED, "b223-unpaired", "NULL".to_string()),
        (PAIRED, "b223-paired", format!("'{PARAM_S1_TEMP_ID}'")),
    ] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO data_streams (id, source_system, source_key, is_active, sensor_id, \
                                           site_parameter_id) \
                 VALUES ('{id}', 'b223', '{key}', true, '{FIXTURE_SENSOR_ID}', {slot})"
            ),
        )
        .await;
    }

    // Both rows as the pre-staging trigger stored them: stamped with the channel's instrument,
    // the paired one attributed and the unpaired one not.
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, time, replicate_index, raw_value, site_id, \
                                   parameter_id, measurement_type) \
             VALUES ('{UNPAIRED}', '{AT}', 0, 1.0, NULL, NULL, 'continuous'), \
                    ('{PAIRED}', '{AT}', 0, 2.0, '{SITE1_ID}', \
                     (SELECT parameter_id FROM site_parameters WHERE id = '{PARAM_S1_TEMP_ID}'), \
                     'continuous')"
        ),
    )
    .await;
    assert_eq!(
        stored(&db, UNPAIRED).await.0,
        Some(FIXTURE_SENSOR_ID.to_string()),
        "the pre-staging trigger stamps an unpaired row"
    );

    crate::common::exec_unprepared(
        &db,
        migration::m20260910_000031_stage_unpaired_readings::UP,
    )
    .await;

    assert_eq!(
        stored(&db, UNPAIRED).await,
        (None, None),
        "the unpaired row is staged: it names no instrument and no curve"
    );
    assert_eq!(
        stored(&db, PAIRED).await.0,
        Some(FIXTURE_SENSOR_ID.to_string()),
        "an attributed row keeps the instrument it was stored with"
    );

    // The CHECK still refuses an attributed row with no instrument, which is the half of it that
    // was never in question. The stream carries none of its own, so the trigger has nothing to
    // fill the column with and the constraint is what answers.
    crate::common::exec(
        &db,
        "INSERT INTO data_streams (id, source_system, source_key, is_active, sensor_id, \
                                   site_parameter_id) \
         VALUES ('00000000-0000-4000-c000-0000000b2233', 'b223', 'b223-bare', true, NULL, \
                 '00000000-0000-4000-a000-000000000101')",
    )
    .await;
    let refused = db
        .execute_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "INSERT INTO readings (stream_id, time, replicate_index, raw_value, site_id, \
                                       parameter_id, sensor_id, measurement_type) \
                 VALUES ('00000000-0000-4000-c000-0000000b2233', '2025-05-02T00:00:00Z', 0, 3.0, \
                         '{SITE1_ID}', \
                         (SELECT parameter_id FROM site_parameters WHERE id = '{PARAM_S1_TEMP_ID}'), \
                         NULL, 'continuous')"
            ),
        ))
        .await;
    assert!(
        refused.is_err(),
        "an attributed reading still may not name no instrument"
    );

    cleanup_test_db(&db).await;
}
