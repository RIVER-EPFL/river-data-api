//! Retiring the calibrations the API minted for itself deletes rows and rewrites the readings that
//! named them, which is the only statement in the change that cannot be undone.
//!
//! Scenario: a database carrying auto-minted identity calibrations, the readings they cover, and
//! two curves an operator entered by hand that a coefficient test would mistake for minted ones.
//! Expected behaviour: a reading that named a minted row comes out naming nothing and serving its
//! raw value, unless a standard curve produced its stored value, in which case that value stands; a
//! curve with real provenance survives whatever its coefficients say; and the minted rows are gone.

use crate::support::{count, exec, fresh_database, migrate_through, scalar};
use sea_orm::DatabaseConnection;
use sea_orm_migration::MigratorTrait;

/// The last migration before the retirement, ie. the schema the fixture is written against.
const BEFORE_RETIREMENT: &str = "m20260813_000006_readings_calibration_id_index";

const STREAM: &str = "11111111-1111-4111-8111-111111111111";
const SENSOR: &str = "22222222-2222-4222-8222-222222222222";
const CURVE: &str = "33333333-3333-4333-8333-333333333333";

const MINTED: &str = "44444444-4444-4444-8444-444444444444";
/// Slope 1.0 and intercept 0.0, entered by a person: identical coefficients, real provenance.
const BY_HAND: &str = "55555555-5555-4555-8555-555555555555";
/// Minted by the system, then annotated: the note mentions the marker phrase without being it.
const ANNOTATED: &str = "66666666-6666-4666-8666-666666666666";

const LOGGED: &str = "2025-06-01T09:00:00Z";
const GRAB: &str = "2025-06-01T10:00:00Z";
const BY_HAND_AT: &str = "2025-06-01T11:00:00Z";
const ANNOTATED_AT: &str = "2025-06-01T12:00:00Z";

async fn seed(db: &DatabaseConnection) {
    exec(
        db,
        &format!(
            "INSERT INTO sensors (id, name, serial_number) \
             VALUES ('{SENSOR}', 'Sonde', 'SN-001')"
        ),
    )
    .await;
    exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key) \
             VALUES ('{STREAM}', 'test', 'sonde')"
        ),
    )
    .await;
    exec(
        db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, name, slope, intercept) \
             VALUES ('{CURVE}', '{SENSOR}', 'Plate A', 3.0, 0.5)"
        ),
    )
    .await;

    for (id, slope, intercept, performed_by, notes, valid_from) in [
        (
            MINTED,
            1.0,
            0.0,
            "system",
            "Identity calibration (auto-created)",
            "2025-01-01T00:00:00Z",
        ),
        (
            BY_HAND,
            1.0,
            0.0,
            "e.thomas",
            "Bench check: the instrument reads true, no correction wanted",
            "2025-02-01T00:00:00Z",
        ),
        (
            ANNOTATED,
            1.0,
            0.0,
            "system",
            "Identity calibration (auto-created), kept deliberately after the March campaign",
            "2025-03-01T00:00:00Z",
        ),
    ] {
        exec(
            db,
            &format!(
                "INSERT INTO sensor_calibrations \
                     (id, sensor_id, slope, intercept, valid_from, performed_by, notes) \
                 VALUES ('{id}', '{SENSOR}', {slope}, {intercept}, '{valid_from}', \
                         '{performed_by}', '{notes}')"
            ),
        )
        .await;
    }
}

async fn insert_reading(
    db: &DatabaseConnection,
    time: &str,
    measurement_type: &str,
    raw: f64,
    calibrated: f64,
    calibration_id: &str,
    standard_curve_id: Option<&str>,
) {
    let curve = standard_curve_id.map_or_else(|| "NULL".to_string(), |id| format!("'{id}'"));
    exec(
        db,
        &format!(
            "INSERT INTO readings \
                 (stream_id, time, replicate_index, raw_value, calibrated_value, sensor_id, \
                  calibration_id, standard_curve_id, measurement_type) \
             VALUES ('{STREAM}', '{time}', 0, {raw}, {calibrated}, '{SENSOR}', \
                     '{calibration_id}', {curve}, '{measurement_type}')"
        ),
    )
    .await;
}

struct StoredReading {
    calibrated_value: Option<f64>,
    calibration_id: Option<String>,
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
    }
}

async fn calibration_exists(db: &DatabaseConnection, id: &str) -> bool {
    count(
        db,
        &format!("SELECT count(*) AS v FROM sensor_calibrations WHERE id = '{id}'"),
    )
    .await
        > 0
}

#[tokio::test]
async fn minted_identity_calibrations_are_retired_and_operator_curves_are_not() {
    let db = fresh_database("river_test_migration_identity").await;
    migrate_through(&db, BEFORE_RETIREMENT).await;

    seed(&db).await;
    insert_reading(&db, LOGGED, "continuous", 5.0, 5.0, MINTED, None).await;
    insert_reading(&db, GRAB, "spot", 10.0, 30.5, MINTED, Some(CURVE)).await;
    insert_reading(&db, BY_HAND_AT, "continuous", 7.0, 7.0, BY_HAND, None).await;
    insert_reading(&db, ANNOTATED_AT, "continuous", 8.0, 8.0, ANNOTATED, None).await;

    migration::Migrator::up(&db, None)
        .await
        .expect("the retirement runs against the populated database");

    let logged = reading_at(&db, LOGGED).await;
    assert_eq!(
        logged.calibration_id, None,
        "a reading that named a minted row names nothing afterwards"
    );
    assert_eq!(
        logged.calibrated_value, None,
        "and carries no corrected value: what it held was its raw value copied, which the reads \
         now serve through COALESCE instead"
    );

    let grab = reading_at(&db, GRAB).await;
    assert_eq!(
        grab.calibration_id, None,
        "the grab drops the minted base with everything else"
    );
    assert_eq!(
        grab.calibrated_value,
        Some(30.5),
        "but its value came from the standard curve it still names, not from the base, so it stands"
    );

    let by_hand = reading_at(&db, BY_HAND_AT).await;
    assert_eq!(
        by_hand.calibration_id.as_deref(),
        Some(BY_HAND),
        "a reading corrected by an operator's curve keeps naming it"
    );
    assert_eq!(by_hand.calibrated_value, Some(7.0));

    let annotated = reading_at(&db, ANNOTATED_AT).await;
    assert_eq!(
        annotated.calibration_id.as_deref(),
        Some(ANNOTATED),
        "and so does one whose curve merely mentions the marker phrase"
    );
    assert_eq!(annotated.calibrated_value, Some(8.0));

    assert!(
        !calibration_exists(&db, MINTED).await,
        "the minted row is deleted, not merely dereferenced"
    );
    assert!(
        calibration_exists(&db, BY_HAND).await,
        "slope 1.0 and intercept 0.0 are a measurement result, not a marker: a curve with an \
         author survives them"
    );
    assert!(
        calibration_exists(&db, ANNOTATED).await,
        "the markers are matched whole, so an annotated note is not one of them"
    );
    assert_eq!(
        count(&db, "SELECT count(*) AS v FROM sensor_calibrations").await,
        2,
        "nothing else left the table"
    );
}
