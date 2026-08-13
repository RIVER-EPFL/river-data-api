//! Standard curves: the hand-picked lab correction that lives in its own table and its own foreign
//! key on a reading.
//!
//! Scenario: an operator fits a curve for a plate, enters the grabs measured against it, and later
//! changes their mind about the instrument's windowed calibration.
//! Expected behaviour: a grab stores the measured value, both curve references and the value the two
//! produce together, in that order; a curve that has been applied is frozen and cannot be deleted or
//! re-fitted in place; and reprocessing the instrument's windowed timeline leaves the grab and its
//! curve exactly as the operator entered them.

use crate::common::sensor_lifecycle::*;
use crate::common::*;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use std::time::Duration;
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(10);

const GRAB_TIME: &str = "2025-06-15T10:00:00Z";
const CONTINUOUS_TIME: &str = "2025-06-15T09:00:00Z";
const BASE_FROM: &str = "2025-01-01T00:00:00Z";

struct Fixture {
    app: axum::Router,
    db: DatabaseConnection,
    token: String,
}

async fn setup() -> Fixture {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    let token = seed_api_token(&db, full_permissions(), None).await;
    let app = build_test_app(db.clone());
    Fixture { app, db, token }
}

/// The whole provenance of one grab: what was measured, which two curves are claimed, and what the
/// pair produced.
#[derive(Debug)]
struct StoredReading {
    raw_value: f64,
    calibrated_value: Option<f64>,
    calibration_id: Option<Uuid>,
    standard_curve_id: Option<Uuid>,
    measurement_type: Option<String>,
}

async fn reading_at(db: &DatabaseConnection, time: &str) -> StoredReading {
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT raw_value, calibrated_value, calibration_id, standard_curve_id, \
                        measurement_type \
                 FROM readings \
                 WHERE parameter_id = '{GLOBAL_PARAM_TEMP_ID}' AND time = '{time}'"
            ),
        ))
        .await
        .expect("query readings")
        .unwrap_or_else(|| panic!("no reading stored at {time}"));
    StoredReading {
        raw_value: row.try_get("", "raw_value").unwrap(),
        calibrated_value: row.try_get("", "calibrated_value").unwrap(),
        calibration_id: row.try_get("", "calibration_id").unwrap(),
        standard_curve_id: row.try_get("", "standard_curve_id").unwrap(),
        measurement_type: row.try_get("", "measurement_type").unwrap(),
    }
}

async fn create_curve(
    fx: &Fixture,
    sensor_id: Uuid,
    name: &str,
    slope: f64,
    intercept: f64,
) -> Uuid {
    let (status, body) = post_json_parse_with_token(
        &fx.app,
        "/api/standard_curves",
        &json!({
            "sensor_id": sensor_id,
            "name": name,
            "slope": slope,
            "intercept": intercept,
        }),
        &fx.token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "creating standard curve '{name}' should succeed: {body}"
    );
    body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("created curve carries no id: {body}"))
        .parse()
        .expect("curve id is a uuid")
}

async fn post_grab(fx: &Fixture, reading: serde_json::Value) -> (u16, String) {
    post_json_with_token(
        &fx.app,
        "/api/grab_samples",
        &json!({ "site_id": SITE1_ID, "readings": [reading] }),
        &fx.token,
    )
    .await
}

fn assert_close(actual: f64, expected: f64, what: &str) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "{what}: expected {expected}, got {actual}"
    );
}

/// A base curve that was resolved and applied is recorded, whatever its coefficients happen to be.
/// Without the second reference, a row whose base was resolved and a row whose base was never
/// resolved would look the same.
#[tokio::test]
#[serial]
async fn grab_with_a_base_curve_and_a_standard_curve_records_both_references() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Microplate-01", GLOBAL_PARAM_TEMP_ID).await;
    let curve = create_curve(&fx, sensor.id, "Plate A", 2.0, 1.0).await;

    let (status, body) = post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "sensor_id": sensor.id,
            "standard_curve_id": curve,
            "value": 10.0,
            "time": GRAB_TIME,
        }),
    )
    .await;
    assert_eq!(status, 200, "grab with a curve should succeed: {body}");

    let stored = reading_at(&fx.db, GRAB_TIME).await;
    assert_close(stored.raw_value, 10.0, "the measured value is kept as raw");
    assert_eq!(
        stored.calibration_id,
        Some(sensor.base_calibration_id),
        "the base calibration the server resolved is the one recorded"
    );
    assert_eq!(
        stored.standard_curve_id,
        Some(curve),
        "the operator's curve is recorded on its own column"
    );
    assert_close(
        stored
            .calibrated_value
            .expect("a curve applied means a value"),
        21.0,
        "the 1:1 base then the curve: 2.0 * (1.0 * 10.0 + 0.0) + 1.0",
    );
    assert_eq!(stored.measurement_type.as_deref(), Some("spot"));
}

/// The instrument correction runs first and the curve maps its output. The order is not recoverable
/// from a stored row, so it is asserted on numbers that differ under the other order.
#[tokio::test]
#[serial]
async fn base_calibration_is_applied_before_the_standard_curve() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Microplate-02", GLOBAL_PARAM_TEMP_ID).await;
    let base = add_calibration(&fx.db, sensor.id, 3.0, 2.0, dt(BASE_FROM)).await;
    let curve = create_curve(&fx, sensor.id, "Plate B", 2.0, 0.5).await;

    let (status, body) = post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "sensor_id": sensor.id,
            "standard_curve_id": curve,
            "value": 10.0,
            "time": GRAB_TIME,
        }),
    )
    .await;
    assert_eq!(status, 200, "grab should succeed: {body}");

    let stored = reading_at(&fx.db, GRAB_TIME).await;
    assert_eq!(
        stored.calibration_id,
        Some(base),
        "the covering windowed calibration is the base, not the bench curve it superseded"
    );
    assert_eq!(stored.standard_curve_id, Some(curve));
    let value = stored.calibrated_value.expect("both curves applied");
    assert_close(
        value,
        64.5,
        "base then curve: 2.0 * (3.0 * 10.0 + 2.0) + 0.5",
    );
    assert!(
        (value - 63.5).abs() > 1e-9,
        "63.5 is the curve-first result, which would mean the order is reversed"
    );
}

/// A curve belongs to the instrument it was fitted on. Accepting another instrument's curve would
/// put a second instrument's coefficients on this measurement, and a curve id that resolves to
/// nothing would silently store an uncorrected value under a correction the operator asked for.
#[tokio::test]
#[serial]
async fn a_curve_from_another_instrument_or_from_nowhere_is_refused() {
    let fx = setup().await;
    let plate_reader = create_sensor(&fx.db, "Microplate-07", GLOBAL_PARAM_TEMP_ID).await;
    let other = create_sensor(&fx.db, "Microplate-08", GLOBAL_PARAM_TEMP_ID).await;
    let curve = create_curve(&fx, other.id, "Plate G", 2.0, 1.0).await;

    let (status, body) = post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "sensor_id": plate_reader.id,
            "standard_curve_id": curve,
            "value": 10.0,
            "time": GRAB_TIME,
        }),
    )
    .await;
    assert_eq!(
        status, 400,
        "a curve fitted on another instrument is refused: {body}"
    );

    let (status, body) = post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "sensor_id": plate_reader.id,
            "standard_curve_id": Uuid::new_v4(),
            "value": 10.0,
            "time": GRAB_TIME,
        }),
    )
    .await;
    assert_eq!(status, 400, "an unknown curve id is refused: {body}");

    let count = fx
        .db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT COUNT(*) AS c FROM readings WHERE time = '{GRAB_TIME}'"),
        ))
        .await
        .expect("count readings")
        .expect("count row");
    assert_eq!(
        count.try_get::<i64>("", "c").unwrap(),
        0,
        "a refused grab stores nothing"
    );
}

/// A zero slope maps every measurement onto the same number, which is a fit that has gone wrong
/// rather than a correction.
#[tokio::test]
#[serial]
async fn a_curve_with_a_zero_slope_is_refused() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Microplate-09", GLOBAL_PARAM_TEMP_ID).await;

    let (status, body) = post_json_with_token(
        &fx.app,
        "/api/standard_curves",
        &json!({
            "sensor_id": sensor.id,
            "name": "Flat plate",
            "slope": 0.0,
            "intercept": 1.0,
        }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 400, "a zero slope is refused: {body}");
}

/// A null `calibrated_value` means no correction was made, and a stamped `calibration_id` means the
/// stored value went through that calibration. Neither may claim the other.
#[tokio::test]
#[serial]
async fn a_grab_does_not_claim_a_calibration_it_did_not_apply() {
    let fx = setup().await;

    let (status, body) = post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "value": 7.5,
            "time": GRAB_TIME,
        }),
    )
    .await;
    assert_eq!(status, 200, "a bare lab value is still a grab: {body}");

    let bare = reading_at(&fx.db, GRAB_TIME).await;
    assert_close(bare.raw_value, 7.5, "the measured value is stored");
    assert_eq!(
        bare.calibrated_value, None,
        "no curve applied, so no corrected value is invented"
    );
    assert_eq!(bare.calibration_id, None, "no calibration is claimed");
    assert_eq!(bare.standard_curve_id, None);

    // The same grab against an instrument whose window covers it: the calibration is recorded
    // because it was applied, and the stored value is the one it produces.
    let sensor = create_sensor(&fx.db, "Microplate-03", GLOBAL_PARAM_TEMP_ID).await;
    let base = add_calibration(&fx.db, sensor.id, 3.0, 2.0, dt(BASE_FROM)).await;
    let (status, body) = post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "sensor_id": sensor.id,
            "value": 10.0,
            "time": CONTINUOUS_TIME,
        }),
    )
    .await;
    assert_eq!(status, 200, "grab without a curve should succeed: {body}");

    let attributed = reading_at(&fx.db, CONTINUOUS_TIME).await;
    assert_eq!(attributed.calibration_id, Some(base));
    assert_eq!(attributed.standard_curve_id, None, "no curve was chosen");
    assert_close(
        attributed
            .calibrated_value
            .expect("a stamped calibration must have been applied"),
        32.0,
        "3.0 * 10.0 + 2.0",
    );
}

/// Deleting a curve a reading was corrected with would leave the reading unable to say where its
/// value came from. The refusal must also leave the reference in place: clearing the foreign key
/// first and then deleting is the same loss with an extra step.
#[tokio::test]
#[serial]
async fn deleting_an_applied_curve_is_refused_and_the_reference_survives() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Microplate-04", GLOBAL_PARAM_TEMP_ID).await;
    let curve = create_curve(&fx, sensor.id, "Plate C", 2.0, 1.0).await;

    let (status, body) = post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "sensor_id": sensor.id,
            "standard_curve_id": curve,
            "value": 10.0,
            "time": GRAB_TIME,
        }),
    )
    .await;
    assert_eq!(status, 200, "grab should succeed: {body}");

    let (status, body) =
        delete_with_token(&fx.app, &format!("/api/standard_curves/{curve}"), &fx.token).await;
    assert_eq!(
        status, 400,
        "deleting an applied curve is refused with a stated reason: {body}"
    );

    let stored = reading_at(&fx.db, GRAB_TIME).await;
    assert_eq!(
        stored.standard_curve_id,
        Some(curve),
        "the reading still points at its curve, ie. the reference was not cleared first"
    );
    assert_close(
        stored.calibrated_value.expect("value unchanged"),
        21.0,
        "the corrected value is untouched",
    );

    let (status, body) =
        get_json_with_token(&fx.app, &format!("/api/standard_curves/{curve}"), &fx.token).await;
    assert_eq!(status, 200, "the curve itself is still there: {body}");
}

/// A curve that has been applied is what a published value was computed from. Re-fitting it in place
/// would rewrite those values with no record; a corrected fit is a new row.
#[tokio::test]
#[serial]
async fn editing_an_applied_curve_is_refused_so_a_new_curve_is_minted() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Microplate-05", GLOBAL_PARAM_TEMP_ID).await;
    let original = create_curve(&fx, sensor.id, "Plate D", 2.0, 1.0).await;

    let (status, body) = post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "sensor_id": sensor.id,
            "standard_curve_id": original,
            "value": 10.0,
            "time": GRAB_TIME,
        }),
    )
    .await;
    assert_eq!(status, 200, "grab should succeed: {body}");

    let (status, body) = put_json_with_token(
        &fx.app,
        &format!("/api/standard_curves/{original}"),
        &json!({ "slope": 5.0 }),
        &fx.token,
    )
    .await;
    assert_eq!(
        status, 400,
        "re-fitting an applied curve is refused: {body}"
    );

    let (status, served) = get_json_with_token(
        &fx.app,
        &format!("/api/standard_curves/{original}"),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "curve still readable: {served}");
    assert_close(
        served["slope"].as_f64().expect("slope is a number"),
        2.0,
        "the coefficients the reading was corrected with are unchanged",
    );

    let refit = create_curve(&fx, sensor.id, "Plate D refit", 5.0, 1.0).await;
    assert_ne!(refit, original, "the corrected fit is a new row");

    let stored = reading_at(&fx.db, GRAB_TIME).await;
    assert_eq!(
        stored.standard_curve_id,
        Some(original),
        "an already-entered measurement keeps the curve it was measured against"
    );
    assert_close(
        stored.calibrated_value.expect("value unchanged"),
        21.0,
        "minting a new curve does not silently restate old values",
    );
}

/// Reprocessing resolves curves by time window. A grab's curve was chosen by hand and no window can
/// recover that choice, so reprocess must leave spot rows alone while still correcting the
/// instrument's continuous history.
#[tokio::test]
#[serial]
async fn reprocessing_the_instrument_leaves_a_grab_and_its_curve_alone() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Sonde-01", GLOBAL_PARAM_TEMP_ID).await;
    let deployment = deploy_sensor(&fx.db, sensor.id, SITE1_ID, dt(BASE_FROM)).await;
    let curve = create_curve(&fx, sensor.id, "Plate E", 2.0, 1.0).await;

    let stream = create_paired_stream(&fx.db, "curve-continuous", PARAM_S1_TEMP_ID).await;
    insert_readings(
        &fx.db,
        stream,
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        sensor.id,
        sensor.base_calibration_id,
        deployment,
        1.0,
        0.0,
        &[(dt(CONTINUOUS_TIME), 10.0)],
    )
    .await;

    let (status, body) = post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "sensor_id": sensor.id,
            "standard_curve_id": curve,
            "value": 10.0,
            "time": GRAB_TIME,
        }),
    )
    .await;
    assert_eq!(status, 200, "grab should succeed: {body}");

    // A new windowed calibration covering both timestamps. Its create hook reprocesses the window.
    let (status, body) = post_json_with_token(
        &fx.app,
        "/api/sensor_calibrations",
        &json!({
            "sensor_id": sensor.id,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "slope": 5.0,
            "intercept": 0.0,
            "valid_from": BASE_FROM,
        }),
        &fx.token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "creating a windowed calibration should succeed: {body}"
    );
    assert!(
        wait_for_reprocessing(&fx.db, sensor.id, WAIT).await,
        "the reprocess job settles without failing"
    );

    let continuous = reading_at(&fx.db, CONTINUOUS_TIME).await;
    assert_close(
        continuous
            .calibrated_value
            .expect("continuous history is recomputed"),
        50.0,
        "5.0 * 10.0 + 0.0",
    );

    let grab = reading_at(&fx.db, GRAB_TIME).await;
    assert_eq!(
        grab.standard_curve_id,
        Some(curve),
        "the hand-picked curve is not replaced by a window resolution"
    );
    assert_eq!(
        grab.calibration_id,
        Some(sensor.base_calibration_id),
        "the base the grab was entered against is not re-derived"
    );
    assert_close(
        grab.calibrated_value.expect("grab keeps its value"),
        21.0,
        "the operator's correction stands",
    );
}

/// The split must not have cost the sensor path anything: editing a windowed calibration still
/// rewrites the readings its window covers.
#[tokio::test]
#[serial]
async fn editing_a_windowed_calibration_still_reprocesses_history() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Sonde-02", GLOBAL_PARAM_TEMP_ID).await;
    let deployment = deploy_sensor(&fx.db, sensor.id, SITE1_ID, dt(BASE_FROM)).await;
    let stream = create_paired_stream(&fx.db, "windowed-history", PARAM_S1_TEMP_ID).await;
    insert_readings(
        &fx.db,
        stream,
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        sensor.id,
        sensor.base_calibration_id,
        deployment,
        1.0,
        0.0,
        &[
            (dt("2025-06-15T09:00:00Z"), 20.0),
            (dt("2025-06-15T09:10:00Z"), 21.5),
        ],
    )
    .await;

    let (status, body) = put_json_with_token(
        &fx.app,
        &format!("/api/sensor_calibrations/{}", sensor.base_calibration_id),
        &json!({ "slope": 3.0, "intercept": 1.0 }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "editing a windowed calibration: {body}");
    assert!(
        wait_for_reprocessing(&fx.db, sensor.id, WAIT).await,
        "the reprocess job settles without failing"
    );

    let rows = get_readings(&fx.db, stream).await;
    assert_eq!(rows.len(), 2);
    assert_close(
        rows[0].calibrated_value.expect("recomputed"),
        61.0,
        "3.0 * 20.0 + 1.0",
    );
    assert_close(
        rows[1].calibrated_value.expect("recomputed"),
        65.5,
        "3.0 * 21.5 + 1.0",
    );
}

/// A served value is only defensible with the curves that produced it, so the export carries both
/// references, and it carries them in every format rather than only the one that was easiest.
#[tokio::test]
#[serial]
async fn the_export_carries_both_curve_references_in_json_and_csv() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Microplate-06", GLOBAL_PARAM_TEMP_ID).await;
    let curve = create_curve(&fx, sensor.id, "Plate F", 2.0, 1.0).await;

    let (status, body) = post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "sensor_id": sensor.id,
            "standard_curve_id": curve,
            "value": 10.0,
            "time": GRAB_TIME,
        }),
    )
    .await;
    assert_eq!(status, 200, "grab should succeed: {body}");

    let query = "start=2025-06-01T00:00:00Z&end=2025-06-30T00:00:00Z&include_curves=true";
    let (status, json) = get_json_with_token(
        &fx.app,
        &format!("/api/sites/{SITE1_ID}/readings?{query}"),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "readings export: {json}");

    let parameter = json["parameters"]
        .as_array()
        .expect("parameters array")
        .iter()
        .find(|p| {
            p["values"]
                .as_array()
                .is_some_and(|v| v.iter().any(|x| !x.is_null()))
        })
        .unwrap_or_else(|| panic!("no parameter carries the grab: {json}"));
    let position = parameter["values"]
        .as_array()
        .unwrap()
        .iter()
        .position(|v| !v.is_null())
        .expect("the grab has a value");
    assert_eq!(
        parameter["standard_curve_ids"][position].as_str(),
        Some(curve.to_string().as_str()),
        "the standard curve reference travels with the point: {parameter}"
    );
    assert_eq!(
        parameter["calibration_ids"][position].as_str(),
        Some(sensor.base_calibration_id.to_string().as_str()),
        "so does the base calibration: {parameter}"
    );

    let code = parameter["code"].as_str().expect("parameter code");
    let (status, csv) = get_csv_with_token(
        &fx.app,
        &format!("/api/sites/{SITE1_ID}/readings?{query}&format=csv"),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "csv export: {csv}");
    let header = csv.lines().next().expect("csv header");
    assert!(
        header.contains(&format!("{code}_standard_curve_id")),
        "csv header names the curve column: {header}"
    );
    assert!(
        header.contains(&format!("{code}_calibration_id")),
        "csv header names the base calibration column: {header}"
    );
    assert!(
        csv.contains(&curve.to_string()),
        "the curve id is in the csv body: {csv}"
    );
}

/// `created_by` records who fitted the curve, so it is supplied on create like every other entity
/// that carries it, and `r_squared` is the fit provenance of a value that has been published, so it
/// is frozen alongside the coefficients once a reading references the curve.
#[tokio::test]
#[serial]
async fn attribution_is_stored_on_create_and_the_fit_quality_freezes_with_the_coefficients() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Microplate-06", GLOBAL_PARAM_TEMP_ID).await;

    let (status, created) = post_json_parse_with_token(
        &fx.app,
        "/api/standard_curves",
        &json!({
            "sensor_id": sensor.id,
            "name": "Plate E",
            "slope": 2.0,
            "intercept": 1.0,
            "r_squared": 0.97,
            "created_by": "lab.tech@epfl.ch",
        }),
        &fx.token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "creating the curve should succeed: {created}"
    );
    assert_eq!(
        created["created_by"].as_str(),
        Some("lab.tech@epfl.ch"),
        "the caller's attribution is stored, not dropped: {created}"
    );
    let curve: Uuid = created["id"]
        .as_str()
        .expect("created curve carries an id")
        .parse()
        .expect("curve id is a uuid");

    let (status, body) = put_json_with_token(
        &fx.app,
        &format!("/api/standard_curves/{curve}"),
        &json!({ "r_squared": 0.99 }),
        &fx.token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "an unused curve can still be re-fitted: {body}"
    );

    let (status, body) = post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "sensor_id": sensor.id,
            "standard_curve_id": curve,
            "value": 10.0,
            "time": GRAB_TIME,
        }),
    )
    .await;
    assert_eq!(status, 200, "grab should succeed: {body}");

    let (status, body) = put_json_with_token(
        &fx.app,
        &format!("/api/standard_curves/{curve}"),
        &json!({ "r_squared": 0.42 }),
        &fx.token,
    )
    .await;
    assert_eq!(
        status, 400,
        "restating the fit quality of an applied curve is refused: {body}"
    );

    let (status, body) = put_json_with_token(
        &fx.app,
        &format!("/api/standard_curves/{curve}"),
        &json!({ "created_by": "someone.else@epfl.ch" }),
        &fx.token,
    )
    .await;
    assert_eq!(
        status, 400,
        "reassigning the attribution of an applied curve is refused: {body}"
    );

    let (status, served) =
        get_json_with_token(&fx.app, &format!("/api/standard_curves/{curve}"), &fx.token).await;
    assert_eq!(status, 200, "curve still readable: {served}");
    assert_close(
        served["r_squared"].as_f64().expect("r_squared is a number"),
        0.99,
        "the fit quality recorded when the value was published is unchanged",
    );
    assert_eq!(
        served["created_by"].as_str(),
        Some("lab.tech@epfl.ch"),
        "the attribution is unchanged: {served}"
    );

    let (status, body) = put_json_with_token(
        &fx.app,
        &format!("/api/standard_curves/{curve}"),
        &json!({ "notes": "plate rerun scheduled" }),
        &fx.token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "notes stay editable on an applied curve: {body}"
    );
}
