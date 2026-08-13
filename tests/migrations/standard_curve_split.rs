//! Splitting standard curves out of `sensor_calibrations` moves rows: the instant calibrations
//! become `standard_curves` and the readings that named them are repointed at the new table.
//!
//! Scenario: a database that has been in use since before the split, so it holds instant
//! calibrations and readings corrected with them.
//! Expected behaviour: every instant curve arrives in `standard_curves` under its own id with its
//! fit intact, the grabs that used it follow it and serve exactly the value they served before,
//! windowed calibrations and their readings are untouched, no logger row comes out carrying a lab
//! curve, and the whole split reverses.

use crate::support::{
    column_exists, count, exec, fresh_database, migrate_through, scalar, steps_back_through,
};
use sea_orm::DatabaseConnection;
use sea_orm_migration::MigratorTrait;

/// The last migration before the split, ie. the schema an existing database is in when it starts.
const BEFORE_SPLIT: &str = "m20260813_000001_calibration_valid_until_explicit";
/// The migration that moves the rows.
const THE_SPLIT: &str = "m20260813_000004_readings_standard_curve_fk";

const STREAM: &str = "11111111-1111-4111-8111-111111111111";
const SENSOR: &str = "22222222-2222-4222-8222-222222222222";
const WINDOWED: &str = "33333333-3333-4333-8333-333333333333";
const PLATE_A: &str = "44444444-4444-4444-8444-444444444444";
const PLATE_B: &str = "55555555-5555-4555-8555-555555555555";

const GRAB_A: &str = "2025-06-01T10:00:00Z";
const GRAB_B: &str = "2025-06-01T11:00:00Z";
const LOGGED: &str = "2025-06-01T09:00:00Z";

/// An instrument, one windowed calibration and two instant ones, in the schema that held both kinds
/// in the same table.
async fn seed_instrument(db: &DatabaseConnection) {
    exec(
        db,
        &format!(
            "INSERT INTO sensors (id, name, serial_number) \
             VALUES ('{SENSOR}', 'Plate reader', 'PR-001')"
        ),
    )
    .await;
    exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key) \
             VALUES ('{STREAM}', 'test', 'plate-reader')"
        ),
    )
    .await;
    exec(
        db,
        &format!(
            "INSERT INTO sensor_calibrations \
                 (id, sensor_id, slope, intercept, valid_from, mode, name) \
             VALUES ('{WINDOWED}', '{SENSOR}', 2.0, 1.0, '2025-01-01T00:00:00Z', 'windowed', 'Bench')"
        ),
    )
    .await;
    exec(
        db,
        &format!(
            "INSERT INTO sensor_calibrations \
                 (id, sensor_id, slope, intercept, valid_from, mode, name, r_squared, notes, created_at) \
             VALUES ('{PLATE_A}', '{SENSOR}', 3.0, 0.5, '2025-06-01T00:00:00Z', 'instant', \
                     'Plate A', 0.998, 'eight point series', '2025-05-30T08:00:00Z')"
        ),
    )
    .await;
    exec(
        db,
        &format!(
            "INSERT INTO sensor_calibrations \
                 (id, sensor_id, slope, intercept, valid_from, mode, created_at) \
             VALUES ('{PLATE_B}', '{SENSOR}', 4.0, 0.0, '2025-06-01T00:00:00Z', 'instant', \
                     '2025-05-30T09:00:00Z')"
        ),
    )
    .await;
}

async fn insert_reading(
    db: &DatabaseConnection,
    time: &str,
    measurement_type: &str,
    raw: f64,
    calibrated: f64,
    calibration_id: &str,
) {
    exec(
        db,
        &format!(
            "INSERT INTO readings \
                 (stream_id, time, replicate_index, raw_value, calibrated_value, sensor_id, \
                  calibration_id, measurement_type) \
             VALUES ('{STREAM}', '{time}', 0, {raw}, {calibrated}, '{SENSOR}', \
                     '{calibration_id}', '{measurement_type}')"
        ),
    )
    .await;
}

struct StoredReading {
    calibrated_value: Option<f64>,
    calibration_id: Option<String>,
    standard_curve_id: Option<String>,
}

async fn reading_at(db: &DatabaseConnection, time: &str) -> StoredReading {
    StoredReading {
        calibrated_value: scalar::<f64>(
            db,
            &format!("SELECT calibrated_value AS v FROM readings WHERE time = '{time}'"),
        )
        .await,
        calibration_id: scalar::<String>(
            db,
            &format!("SELECT calibration_id::text AS v FROM readings WHERE time = '{time}'"),
        )
        .await,
        standard_curve_id: scalar::<String>(
            db,
            &format!("SELECT standard_curve_id::text AS v FROM readings WHERE time = '{time}'"),
        )
        .await,
    }
}

#[tokio::test]
async fn instant_calibrations_become_standard_curves_and_their_readings_follow() {
    let db = fresh_database("river_test_migration_split").await;
    migrate_through(&db, BEFORE_SPLIT).await;

    seed_instrument(&db).await;
    insert_reading(&db, GRAB_A, "spot", 10.0, 30.5, PLATE_A).await;
    insert_reading(&db, GRAB_B, "spot", 2.0, 8.0, PLATE_B).await;
    insert_reading(&db, LOGGED, "continuous", 5.0, 11.0, WINDOWED).await;

    migration::Migrator::up(&db, None)
        .await
        .expect("the rest of the migrations run against the populated database");

    let carried = scalar::<String>(
        &db,
        &format!(
            "SELECT (slope || '/' || intercept || '/' || r_squared || '/' || name || '/' || notes \
                     || '/' || created_at) AS v \
             FROM standard_curves WHERE id = '{PLATE_A}'"
        ),
    )
    .await
    .expect("the instant calibration arrives as a standard curve under its own id");
    assert!(
        carried.starts_with("3/0.5/0.998/Plate A/eight point series/2025-05-30 08:00:00"),
        "the fit moves across whole: {carried}"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS v FROM standard_curves WHERE id = '{PLATE_B}' AND name IS NULL"
            ),
        )
        .await,
        1,
        "a curve that was never named moves too"
    );

    let grab_a = reading_at(&db, GRAB_A).await;
    assert_eq!(
        grab_a.standard_curve_id.as_deref(),
        Some(PLATE_A),
        "the grab points at the curve in its new table"
    );
    assert_eq!(
        grab_a.calibration_id, None,
        "and no longer claims it as a base calibration"
    );
    assert_eq!(
        grab_a.calibrated_value,
        Some(30.5),
        "the move repoints a reference, it does not recompute a value"
    );

    let grab_b = reading_at(&db, GRAB_B).await;
    assert_eq!(grab_b.standard_curve_id.as_deref(), Some(PLATE_B));
    assert_eq!(grab_b.calibrated_value, Some(8.0));

    let logged = reading_at(&db, LOGGED).await;
    assert_eq!(
        logged.calibration_id.as_deref(),
        Some(WINDOWED),
        "a windowed calibration is not part of the move"
    );
    assert_eq!(logged.standard_curve_id, None);
    assert_eq!(logged.calibrated_value, Some(11.0));

    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS v FROM sensor_calibrations \
                 WHERE id IN ('{PLATE_A}', '{PLATE_B}')"
            ),
        )
        .await,
        0,
        "the instant rows leave the calibration table rather than being copied out of it"
    );
    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) AS v FROM sensor_calibrations WHERE id = '{WINDOWED}'"),
        )
        .await,
        1,
        "the windowed row stays"
    );
    assert!(
        !column_exists(&db, "sensor_calibrations", "mode").await,
        "with the two kinds in two tables the discriminator has nothing left to discriminate"
    );

    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS v FROM pg_constraint \
             WHERE conname = 'readings_standard_curve_id_fkey'",
        )
        .await,
        1,
        "the new reference is a foreign key, so a curve a reading uses cannot be dropped"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS v FROM pg_indexes \
             WHERE indexname = 'idx_readings_standard_curve_id'",
        )
        .await,
        1,
        "and it is indexed, or that check scans the hypertable on every curve delete"
    );
}

/// A standard curve is a grab's correction. A logger row that named an instant calibration must not
/// come out of the move carrying one: no write path will create that state, and reprocess would
/// rewrite the row's value from its window while leaving the curve reference beside it standing.
#[tokio::test]
async fn a_non_spot_reading_does_not_survive_the_move_carrying_a_standard_curve() {
    let db = fresh_database("river_test_migration_nonspot").await;
    migrate_through(&db, BEFORE_SPLIT).await;

    seed_instrument(&db).await;
    insert_reading(&db, LOGGED, "continuous", 5.0, 15.5, PLATE_A).await;
    insert_reading(&db, GRAB_A, "spot", 10.0, 30.5, PLATE_A).await;

    migration::Migrator::up(&db, None)
        .await
        .expect("the rest of the migrations run against the populated database");

    let logged = reading_at(&db, LOGGED).await;
    assert_eq!(
        logged.standard_curve_id, None,
        "a continuous reading does not acquire a lab curve"
    );
    assert_eq!(
        logged.calibration_id, None,
        "and drops the reference to a row that is no longer a calibration"
    );
    assert_eq!(
        logged.calibrated_value,
        Some(15.5),
        "its value is left for window resolution to re-derive, not changed here"
    );

    assert_eq!(
        reading_at(&db, GRAB_A).await.standard_curve_id.as_deref(),
        Some(PLATE_A),
        "the grab beside it still follows its curve"
    );
}

/// The rollback half of a coordinated deploy: the curves go back where they came from and the
/// grabs name them again, and re-applying the split lands on the same state as before.
#[tokio::test]
async fn the_split_reverses_cleanly() {
    let db = fresh_database("river_test_migration_reverse").await;
    migration::Migrator::up(&db, None)
        .await
        .expect("migrate a fresh database all the way");

    exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, serial_number) \
             VALUES ('{SENSOR}', 'Plate reader', 'PR-001')"
        ),
    )
    .await;
    exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key) \
             VALUES ('{STREAM}', 'test', 'plate-reader')"
        ),
    )
    .await;
    exec(
        &db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, name, slope, intercept, r_squared) \
             VALUES ('{PLATE_A}', '{SENSOR}', 'Plate A', 3.0, 0.5, 0.998)"
        ),
    )
    .await;
    exec(
        &db,
        &format!(
            "INSERT INTO readings \
                 (stream_id, time, replicate_index, raw_value, calibrated_value, sensor_id, \
                  standard_curve_id, measurement_type) \
             VALUES ('{STREAM}', '{GRAB_A}', 0, 10.0, 30.5, '{SENSOR}', '{PLATE_A}', 'spot')"
        ),
    )
    .await;

    migration::Migrator::down(&db, Some(steps_back_through(THE_SPLIT)))
        .await
        .expect("roll the split back");

    assert!(
        !column_exists(&db, "readings", "standard_curve_id").await,
        "the second reference is gone"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS v FROM sensor_calibrations \
                 WHERE id = '{PLATE_A}' AND mode = 'instant' AND slope = 3.0 AND intercept = 0.5"
            ),
        )
        .await,
        1,
        "the curve is a calibration again, under the same id"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS v FROM readings \
                 WHERE time = '{GRAB_A}' AND calibration_id = '{PLATE_A}'"
            ),
        )
        .await,
        1,
        "and the grab names it the only way the old schema can"
    );

    migration::Migrator::up(&db, None)
        .await
        .expect("re-apply the split over the rolled-back state");

    let grab = reading_at(&db, GRAB_A).await;
    assert_eq!(
        grab.standard_curve_id.as_deref(),
        Some(PLATE_A),
        "the round trip lands where it started"
    );
    assert_eq!(grab.calibration_id, None);
    assert_eq!(grab.calibrated_value, Some(30.5));
    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) AS v FROM standard_curves WHERE id = '{PLATE_A}'"),
        )
        .await,
        1,
        "the curve exists once, not once per round trip"
    );
}
