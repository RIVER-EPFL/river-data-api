//! The instrument on a hand-entered value is declared, never implied: the slot says what measures
//! it, and the instrument a chosen curve belongs to travels only with the rows that curve
//! corrected.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

const LAB_INSTRUMENT: &str = "00000000-0000-4000-c000-0000000000b1";
const SLOT_INSTRUMENT: &str = "00000000-0000-4000-c000-0000000000b2";
const CURVE: &str = "00000000-0000-4000-c000-0000000000b3";
const TIME: &str = "2025-08-04T09:00:00Z";

async fn instrument_of(db: &DatabaseConnection, parameter_id: &str) -> Option<Uuid> {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT sensor_id FROM readings \
             WHERE parameter_id = '{parameter_id}' AND time = '{TIME}'"
        ),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<Option<Uuid>>("", "sensor_id")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn one_curve_stamps_its_own_rows_and_the_slot_declares_the_rest() {
    let f = crate::common::seeded_app().await;
    let (app, token, db) = (f.app, f.token, f.db);

    for (id, name) in [
        (LAB_INSTRUMENT, "Plate reader"),
        (SLOT_INSTRUMENT, "Field probe"),
    ] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO sensors (id, name, is_active, is_lab_instrument, created_at) \
                 VALUES ('{id}', '{name}', true, true, now())"
            ),
        )
        .await;
    }
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept, name) \
             VALUES ('{CURVE}', '{LAB_INSTRUMENT}', 2.0, 1.0, 'Plate A')"
        ),
    )
    .await;
    // The second parameter's slot declares what measures it; the first declares nothing.
    crate::common::exec(
        &db,
        &format!(
            "UPDATE site_parameters SET instrument_sensor_id = '{SLOT_INSTRUMENT}' WHERE id = '{}'",
            crate::common::PARAM_S1_DO_ID
        ),
    )
    .await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "created_by": "lab",
            "readings": [
                { "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID, "sensor_id": LAB_INSTRUMENT,
                  "standard_curve_id": CURVE, "value": 10.0, "time": TIME },
                { "parameter_id": crate::common::GLOBAL_PARAM_DO_ID, "value": 4.5, "time": TIME }
            ]
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "save ({status}): {body}");

    assert_eq!(
        instrument_of(&db, crate::common::GLOBAL_PARAM_TEMP_ID).await,
        Some(Uuid::parse_str(LAB_INSTRUMENT).unwrap()),
        "the row the curve corrected names the instrument that curve was fitted on"
    );
    assert_eq!(
        instrument_of(&db, crate::common::GLOBAL_PARAM_DO_ID).await,
        Some(Uuid::parse_str(SLOT_INSTRUMENT).unwrap()),
        "the other parameter names what its slot declares, not the curve's instrument"
    );
}

#[tokio::test]
#[serial]
async fn a_curve_fitted_on_another_instrument_is_refused_against_the_declaration() {
    let f = crate::common::seeded_app().await;
    let (app, token, db) = (f.app, f.token, f.db);

    for (id, name) in [
        (LAB_INSTRUMENT, "Plate reader"),
        (SLOT_INSTRUMENT, "Field probe"),
    ] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO sensors (id, name, is_active, is_lab_instrument, created_at) \
                 VALUES ('{id}', '{name}', true, true, now())"
            ),
        )
        .await;
    }
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept, name) \
             VALUES ('{CURVE}', '{LAB_INSTRUMENT}', 2.0, 1.0, 'Plate A')"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE site_parameters SET instrument_sensor_id = '{SLOT_INSTRUMENT}' WHERE id = '{}'",
            crate::common::PARAM_S1_TEMP_ID
        ),
    )
    .await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "created_by": "lab",
            "readings": [
                { "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
                  "standard_curve_id": CURVE, "value": 10.0, "time": TIME }
            ]
        }),
        &token,
    )
    .await;
    assert_eq!(
        status, 400,
        "a curve fitted on another instrument than the slot declares is refused: {body}"
    );
}
