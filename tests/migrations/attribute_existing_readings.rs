//! Scenario: a database carried forward by the migration chain holds readings that predate the
//! instrument rule, and the rule is added to it.
//!
//! Expected behaviour: every non-derived reading ends up naming the instrument its stream names,
//! derived readings end up declaring what they are and stay instrument-less, and only then does
//! the CHECK exist.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use crate::common::{cleanup_test_db, seed_test_data, setup_test_db, FIXTURE_SENSOR_ID, SITE1_ID};

/// Undo the rule the baseline creates with the table, leaving the shape a chain-carried database
/// has: no CHECK, no inheriting trigger, and no fixture instrument defaulted onto new streams.
async fn revert_to_pre_rule_shape(db: &DatabaseConnection) {
    for sql in [
        "ALTER TABLE readings DROP CONSTRAINT IF EXISTS readings_instrument_required",
        "DROP TRIGGER IF EXISTS trg_readings_inherit_stream_instrument ON readings",
        "ALTER TABLE data_streams ALTER COLUMN sensor_id DROP DEFAULT",
    ] {
        db.execute_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            sql.to_string(),
        ))
        .await
        .expect("revert to the pre-rule shape");
    }
}

/// Put back the column default `setup_test_db` gives hand-built fixture streams, and the CHECK as
/// `m20260906_000002_instrument_required_untyped` tightened it, so the shared database is left as
/// the next suite expects it. The migration under test adds the rule in its pre-tightening form,
/// which admits a reading whose `measurement_type` is NULL.
async fn restore_fixture_default(db: &DatabaseConnection) {
    crate::common::exec(
        db,
        "ALTER TABLE readings DROP CONSTRAINT IF EXISTS readings_instrument_required",
    )
    .await;
    crate::common::exec(
        db,
        "ALTER TABLE readings ADD CONSTRAINT readings_instrument_required \
         CHECK ((sensor_id IS NOT NULL) OR (measurement_type IS NOT DISTINCT FROM 'derived'))",
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "ALTER TABLE data_streams ALTER COLUMN sensor_id SET DEFAULT '{FIXTURE_SENSOR_ID}'"
        ),
    )
    .await;
}

/// The migration's own SQL, run as the migrator runs it: several statements in one implicit
/// transaction, which is what `SET LOCAL` and the temporary table need.
async fn run_migration(db: &DatabaseConnection) {
    db.execute_unprepared(
        &migration::m20260906_000001_attribute_existing_readings::attribute_existing_readings(),
    )
    .await
    .expect("the repair applies");
}


async fn scalar(db: &DatabaseConnection, sql: &str) -> Option<String> {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            sql.to_string(),
        ))
        .await
        .expect("query")
        .expect("row");
    row.try_get::<Option<String>>("", "v").expect("column v")
}

const API_STREAM: &str = "00000000-0000-4000-c000-0000000009a1";
const DERIVED_STREAM: &str = "00000000-0000-4000-c000-0000000009d1";
const SITE_PARAM: &str = "00000000-0000-4000-a000-000000000101";

/// Build the two shapes the prod rehearsal holds: an `api` channel whose stream and readings name
/// no instrument, and a `derived` stream whose readings predate `measurement_type` being stamped.
async fn seed_unattributed_readings(db: &DatabaseConnection) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, \
             site_parameter_id, is_active, sensor_id) VALUES \
             ('{API_STREAM}', 'api', '{SITE1_ID}:{SITE_PARAM}', 'API batch insert', \
              '{SITE_PARAM}', true, NULL), \
             ('{DERIVED_STREAM}', 'derived', 'DOmgL_{SITE1_ID}', 'DOmgL', NULL, true, NULL)"
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, time, replicate_index, raw_value, site_id, \
             parameter_id, sensor_id, measurement_type) VALUES \
             ('{API_STREAM}', '2026-01-01T00:00:00Z', 0, 1.5, '{SITE1_ID}', \
              (SELECT parameter_id FROM site_parameters WHERE id = '{SITE_PARAM}'), NULL, 'continuous'), \
             ('{DERIVED_STREAM}', '2026-01-01T00:00:00Z', 0, 8.25, '{SITE1_ID}', \
              (SELECT parameter_id FROM site_parameters WHERE id = '{SITE_PARAM}'), NULL, NULL)"
        ),
    )
    .await;
}

#[tokio::test]
#[serial]
async fn attributes_every_reading_before_adding_the_constraint() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_test_data(&db).await;
    revert_to_pre_rule_shape(&db).await;
    seed_unattributed_readings(&db).await;

    // Without the repair the CHECK cannot be added at all: both rows refuse it.
    let refused = db
        .execute_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "ALTER TABLE readings ADD CONSTRAINT readings_instrument_required \
             CHECK ((sensor_id IS NOT NULL) OR (measurement_type = 'derived'))"
                .to_string(),
        ))
        .await;
    assert!(
        refused.is_err(),
        "the unrepaired rows should refuse the constraint"
    );

    run_migration(&db).await;

    let stream_instrument = scalar(
        &db,
        &format!("SELECT sensor_id::text AS v FROM data_streams WHERE id = '{API_STREAM}'"),
    )
    .await
    .expect("the api stream is given an instrument");
    let reading_instrument = scalar(
        &db,
        &format!("SELECT sensor_id::text AS v FROM readings WHERE stream_id = '{API_STREAM}'"),
    )
    .await
    .expect("the api reading is stamped");
    assert_eq!(
        stream_instrument, reading_instrument,
        "a reading names the instrument its stream names"
    );

    // The identity is the one `resolve_or_mint_stream_instrument` applies: a non-device feed takes
    // the source's instrument for the channel it carries.
    assert_eq!(
        scalar(
            &db,
            &format!(
                "SELECT source_system || '|' || source_key AS v FROM sensors \
                 WHERE id = '{stream_instrument}'"
            )
        )
        .await
        .as_deref(),
        Some(format!("api|api:{SITE1_ID}:{SITE_PARAM}").as_str())
    );

    assert_eq!(
        scalar(
            &db,
            &format!(
                "SELECT measurement_type AS v FROM readings WHERE stream_id = '{DERIVED_STREAM}'"
            )
        )
        .await
        .as_deref(),
        Some("derived"),
        "a derived reading declares what it is rather than borrowing an instrument"
    );
    assert_eq!(
        scalar(
            &db,
            &format!("SELECT sensor_id::text AS v FROM readings WHERE stream_id = '{DERIVED_STREAM}'")
        )
        .await,
        None
    );

    // The rule is now present and refuses what it exists to refuse.
    crate::common::exec(
        &db,
        "INSERT INTO data_streams (id, source_system, source_key, is_active, sensor_id) \
         VALUES ('00000000-0000-4000-c000-0000000009f1', 'api', 'orphan', true, NULL)",
    )
    .await;
    let refused = db
        .execute_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO readings (stream_id, time, replicate_index, raw_value, measurement_type) \
             VALUES ('00000000-0000-4000-c000-0000000009f1', '2026-02-01T00:00:00Z', 0, 2.0, \
                     'continuous')"
                .to_string(),
        ))
        .await;
    assert!(
        refused.is_err(),
        "a continuous reading with no instrument to inherit is refused"
    );

    restore_fixture_default(&db).await;
    cleanup_test_db(&db).await;
}

/// Running it twice must be a no-op: the migrator may be pointed at a database that already holds
/// the rule, and a second pass must not mint a second instrument for the same channel.
#[tokio::test]
#[serial]
async fn is_idempotent() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_test_data(&db).await;
    revert_to_pre_rule_shape(&db).await;
    seed_unattributed_readings(&db).await;

    run_migration(&db).await;
    let first = scalar(&db, "SELECT count(*)::text AS v FROM sensors").await;
    run_migration(&db).await;
    let second = scalar(&db, "SELECT count(*)::text AS v FROM sensors").await;

    assert_eq!(first, second, "a second pass mints nothing");

    restore_fixture_default(&db).await;
    cleanup_test_db(&db).await;
}
