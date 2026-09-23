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

const ENTRY_CHANNEL: &str = "00000000-0000-4000-c000-0000000000b4";

/// A bookkeeping row is minted so a reading can name something where nothing was declared, so
/// naming one as what measures a slot records the absence of an answer as an answer.
#[tokio::test]
#[serial]
async fn a_bookkeeping_instrument_cannot_be_declared_as_what_measures_a_slot() {
    let f = crate::common::seeded_app().await;
    let (app, token, db) = (f.app, f.token, f.db);

    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, is_active, is_lab_instrument, kind, source_system, source_key, created_at) \
             VALUES ('{ENTRY_CHANNEL}', 'Saxon Water Temperature (grab entry)', true, true, \
                     'entry_channel', 'grab_sample', 'saxon:temp', now())"
        ),
    )
    .await;

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/site_parameters/{}", crate::common::PARAM_S1_TEMP_ID),
        &serde_json::json!({ "instrument_sensor_id": ENTRY_CHANNEL }),
        &token,
    )
    .await;
    assert_eq!(
        status, 400,
        "the declaration refuses an entry channel: {body}"
    );
    assert!(
        body.contains("entry_channel"),
        "the refusal says what the row is: {body}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sensors/{ENTRY_CHANNEL}/adopt"),
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "adopt refuses an entry channel: {body}");
}

/// The declaration itself still works: a device is what the guard admits.
#[tokio::test]
#[serial]
async fn a_device_is_still_accepted_as_what_measures_a_slot() {
    let f = crate::common::seeded_app().await;
    let (app, token, db) = (f.app, f.token, f.db);

    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, is_active, kind, created_at) \
             VALUES ('{SLOT_INSTRUMENT}', 'Field probe', true, 'device', now())"
        ),
    )
    .await;

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/site_parameters/{}", crate::common::PARAM_S1_TEMP_ID),
        &serde_json::json!({ "instrument_sensor_id": SLOT_INSTRUMENT }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "a device is admitted: {body}");
}

const RETIRED_INSTRUMENT: &str = "00000000-0000-4000-c000-0000000000b5";

/// An instrument marked inactive has left the lab. Naming it at a write would have window
/// resolution attribute every later reading to something that is gone, so both writes that name
/// one refuse it. Readings stay unguarded: a visit entered from paper after the retirement lands.
#[tokio::test]
#[serial]
async fn a_retired_instrument_cannot_be_declared_or_deployed() {
    let f = crate::common::seeded_app().await;
    let (app, token, db) = (f.app, f.token, f.db);

    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, is_active, kind, created_at) \
             VALUES ('{RETIRED_INSTRUMENT}', 'miniDOT 7392', false, 'device', now())"
        ),
    )
    .await;

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/site_parameters/{}", crate::common::PARAM_S1_TEMP_ID),
        &serde_json::json!({ "instrument_sensor_id": RETIRED_INSTRUMENT }),
        &token,
    )
    .await;
    assert_eq!(
        status, 400,
        "the declaration refuses a retired instrument: {body}"
    );
    assert!(
        body.contains("retired"),
        "the refusal says the instrument is retired: {body}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sensors/{RETIRED_INSTRUMENT}/adopt"),
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "adopt refuses a retired instrument: {body}");
}

/// A bookkeeping row records that nothing was declared, so it is not an answer to "what measured
/// this", whichever list the operator picked it from. Same for a retired instrument.
#[tokio::test]
#[serial]
async fn a_grab_cannot_name_a_bookkeeping_row_or_a_retired_instrument() {
    let f = crate::common::seeded_app().await;
    let (app, token, db) = (f.app, f.token, f.db);

    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, kind, is_active, created_at) \
             VALUES ('{LAB_INSTRUMENT}', 'CNET DOC channel', 'entry_channel', true, now()), \
                    ('{SLOT_INSTRUMENT}', 'Retired probe', 'device', false, now())"
        ),
    )
    .await;

    for (instrument, expected) in [
        (LAB_INSTRUMENT, "entry_channel"),
        (SLOT_INSTRUMENT, "retired"),
    ] {
        let (status, body) = crate::common::post_json_with_token(
            &app,
            "/api/grab_samples",
            &serde_json::json!({
                "site_id": crate::common::SITE1_ID,
                "readings": [
                    { "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID, "sensor_id": instrument,
                      "value": 10.0, "time": TIME }
                ]
            }),
            &token,
        )
        .await;
        assert_eq!(status, 400, "save ({status}): {body}");
        assert!(body.contains(expected), "the refusal says why: {body}");
    }

    let stored = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT count(*) AS n FROM readings WHERE time = '{TIME}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "n")
        .unwrap();
    assert_eq!(stored, 0, "nothing was stored");
}
