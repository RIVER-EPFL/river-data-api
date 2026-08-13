//! What a grab's stored value follows once it has been entered.
//!
//! Scenario: an operator enters a grab against an instrument, then corrects that instrument's
//! windowed calibration.
//! Expected behaviour: the grab serves what the curves it names produce now, so the value and the
//! two curve references beside it always agree; no window resolution is ever imposed on a grab, so
//! a curve the grab does not carry never reaches it; and a grab that resolved no curve at all keeps
//! a null value, which is what says no correction was made, unless a caller supplied a corrected
//! number with no curve behind it, which is reported rather than overwritten.

use crate::common::e2e;
use crate::common::sensor_lifecycle::*;
use crate::common::*;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use std::time::Duration;
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(10);

const EARLY_GRAB: &str = "2024-06-01T10:00:00Z";
const LATE_GRAB: &str = "2025-06-01T10:00:00Z";
const CONTINUOUS_TIME: &str = "2025-06-01T09:00:00Z";
const DEPLOY_FROM: &str = "2024-01-01T00:00:00Z";
const CURVE_FROM: &str = "2025-01-01T00:00:00Z";

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

struct StoredGrab {
    calibrated_value: Option<f64>,
    calibration_id: Option<Uuid>,
    standard_curve_id: Option<Uuid>,
}

async fn grab_at(db: &DatabaseConnection, time: &str) -> StoredGrab {
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT calibrated_value, calibration_id, standard_curve_id FROM readings \
                 WHERE time = '{time}' AND measurement_type = 'spot'"
            ),
        ))
        .await
        .expect("query readings")
        .unwrap_or_else(|| panic!("no grab stored at {time}"));
    StoredGrab {
        calibrated_value: row.try_get("", "calibrated_value").unwrap(),
        calibration_id: row.try_get("", "calibration_id").unwrap(),
        standard_curve_id: row.try_get("", "standard_curve_id").unwrap(),
    }
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

async fn create_standard_curve(fx: &Fixture, sensor_id: Uuid, slope: f64, intercept: f64) -> Uuid {
    let (status, body) = post_json_parse_with_token(
        &fx.app,
        "/api/standard_curves",
        &json!({
            "sensor_id": sensor_id,
            "name": "Plate",
            "slope": slope,
            "intercept": intercept,
        }),
        &fx.token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "creating a standard curve should succeed: {body}"
    );
    body["id"].as_str().unwrap().parse().unwrap()
}

async fn edit_calibration(fx: &Fixture, id: Uuid, patch: serde_json::Value) -> (u16, String) {
    put_json_with_token(
        &fx.app,
        &format!("/api/sensor_calibrations/{id}"),
        &patch,
        &fx.token,
    )
    .await
}

/// Whether the curve's end date is recorded as an operator's rather than the window chain's.
async fn window_provenance(db: &DatabaseConnection, calibration_id: Uuid) -> bool {
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT valid_until_explicit FROM sensor_calibrations WHERE id = '{calibration_id}'"
        ),
    ))
    .await
    .expect("query calibration")
    .expect("the curve is there")
    .try_get::<bool>("", "valid_until_explicit")
    .unwrap()
}

fn assert_close(actual: f64, expected: f64, what: &str) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "{what}: expected {expected}, got {actual}"
    );
}

/// A grab records the base calibration it was corrected with. Editing that calibration's
/// coefficients without moving the grab would leave the served value and the provenance beside it
/// describing two different corrections.
#[tokio::test]
#[serial]
async fn editing_a_base_calibration_moves_the_grab_it_corrected() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Plate-reader-01", GLOBAL_PARAM_TEMP_ID).await;
    let base = add_calibration(&fx.db, sensor.id, 2.0, 0.0, dt(CURVE_FROM)).await;

    let (status, body) = post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "sensor_id": sensor.id,
            "value": 10.0,
            "time": LATE_GRAB,
        }),
    )
    .await;
    assert_eq!(status, 200, "grab should succeed: {body}");

    let entered = grab_at(&fx.db, LATE_GRAB).await;
    assert_eq!(entered.calibration_id, Some(base));
    assert_close(
        entered.calibrated_value.expect("the base was applied"),
        20.0,
        "2.0 * 10.0 + 0.0",
    );

    let (status, body) = edit_calibration(&fx, base, json!({ "slope": 3.0 })).await;
    assert_eq!(status, 200, "editing the base calibration: {body}");
    assert!(
        wait_for_reprocessing(&fx.db, sensor.id, WAIT).await,
        "the reprocess job settles without failing"
    );

    let after = grab_at(&fx.db, LATE_GRAB).await;
    assert_close(
        after.calibrated_value.expect("still corrected"),
        30.0,
        "the corrected value follows the coefficients it claims: 3.0 * 10.0 + 0.0",
    );
    assert_eq!(
        after.calibration_id,
        Some(base),
        "and it still names the calibration that produced it"
    );
}

/// The standard curve is applied to what the base produced, so a base edit moves the composed value
/// and the operator's curve stays exactly where it was put.
#[tokio::test]
#[serial]
async fn a_base_edit_recomposes_the_operators_curve_on_top() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Plate-reader-02", GLOBAL_PARAM_TEMP_ID).await;
    let base = add_calibration(&fx.db, sensor.id, 2.0, 0.0, dt(CURVE_FROM)).await;
    let curve = create_standard_curve(&fx, sensor.id, 3.0, 1.0).await;

    let (status, body) = post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "sensor_id": sensor.id,
            "standard_curve_id": curve,
            "value": 10.0,
            "time": LATE_GRAB,
        }),
    )
    .await;
    assert_eq!(status, 200, "grab should succeed: {body}");
    assert_close(
        grab_at(&fx.db, LATE_GRAB)
            .await
            .calibrated_value
            .expect("both curves applied"),
        61.0,
        "3.0 * (2.0 * 10.0) + 1.0",
    );

    let (status, body) = edit_calibration(&fx, base, json!({ "slope": 5.0 })).await;
    assert_eq!(status, 200, "editing the base calibration: {body}");
    assert!(
        wait_for_reprocessing(&fx.db, sensor.id, WAIT).await,
        "the reprocess job settles without failing"
    );

    let after = grab_at(&fx.db, LATE_GRAB).await;
    assert_close(
        after.calibrated_value.expect("still corrected"),
        151.0,
        "base then curve, with the new base: 3.0 * (5.0 * 10.0) + 1.0",
    );
    assert_eq!(
        after.standard_curve_id,
        Some(curve),
        "the hand-picked curve is unchanged"
    );
    assert_eq!(after.calibration_id, Some(base));
}

/// A grab's base is resolved once, at entry. Editing a different curve on the same instrument
/// reprocesses the continuous history that curve's window covers, and must not reach across to a
/// grab that carries a different base.
#[tokio::test]
#[serial]
async fn a_windowed_edit_elsewhere_leaves_the_grab_alone() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Sonde-10", GLOBAL_PARAM_TEMP_ID).await;
    let deployment = deploy_sensor(&fx.db, sensor.id, SITE1_ID, dt(DEPLOY_FROM)).await;
    let later = add_calibration(&fx.db, sensor.id, 2.0, 0.0, dt(CURVE_FROM)).await;
    let curve = create_standard_curve(&fx, sensor.id, 3.0, 0.0).await;

    let stream = create_paired_stream(&fx.db, "recomposition-continuous", PARAM_S1_TEMP_ID).await;
    insert_readings(
        &fx.db,
        stream,
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        sensor.id,
        later,
        deployment,
        2.0,
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
            "time": EARLY_GRAB,
        }),
    )
    .await;
    assert_eq!(status, 200, "grab should succeed: {body}");
    let entered = grab_at(&fx.db, EARLY_GRAB).await;
    assert_eq!(
        entered.calibration_id,
        Some(sensor.base_calibration_id),
        "the grab predates the later curve, so the bench base is the window it resolves"
    );
    assert_close(
        entered.calibrated_value.expect("the curve was applied"),
        30.0,
        "3.0 * (1.0 * 10.0) + 0.0",
    );

    let (status, body) = edit_calibration(&fx, later, json!({ "slope": 7.0 })).await;
    assert_eq!(status, 200, "editing the later calibration: {body}");
    assert!(
        wait_for_reprocessing(&fx.db, sensor.id, WAIT).await,
        "the reprocess job settles without failing"
    );

    let rows = get_readings(&fx.db, stream).await;
    assert_close(
        rows[0].calibrated_value.expect("recomputed"),
        70.0,
        "the continuous reading in that window moves: 7.0 * 10.0",
    );

    let after = grab_at(&fx.db, EARLY_GRAB).await;
    assert_eq!(
        after.calibration_id,
        Some(sensor.base_calibration_id),
        "no window resolution is imposed on a grab"
    );
    assert_eq!(after.standard_curve_id, Some(curve));
    assert_close(
        after.calibrated_value.expect("value kept"),
        30.0,
        "and its value is untouched",
    );
}

/// A null `calibrated_value` is the record that nothing corrected the measurement. A later
/// calibration covering the same instant does not retrospectively correct a grab, so the null
/// stands.
#[tokio::test]
#[serial]
async fn a_grab_that_resolved_no_curve_keeps_a_null_value() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Plate-reader-03", GLOBAL_PARAM_TEMP_ID).await;
    delete_calibration(&fx.db, sensor.base_calibration_id).await;

    let (status, body) = post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "sensor_id": sensor.id,
            "value": 10.0,
            "time": LATE_GRAB,
        }),
    )
    .await;
    assert_eq!(status, 200, "grab should succeed: {body}");

    let entered = grab_at(&fx.db, LATE_GRAB).await;
    assert_eq!(entered.calibration_id, None, "no curve covered the grab");
    assert_eq!(
        entered.calibrated_value, None,
        "so no corrected value is invented"
    );

    let (status, body) = post_json_with_token(
        &fx.app,
        "/api/sensor_calibrations",
        &json!({
            "sensor_id": sensor.id,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "slope": 4.0,
            "intercept": 0.0,
            "valid_from": CURVE_FROM,
        }),
        &fx.token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "creating a calibration should succeed: {body}"
    );
    assert!(
        wait_for_reprocessing(&fx.db, sensor.id, WAIT).await,
        "the reprocess job settles without failing"
    );

    let after = grab_at(&fx.db, LATE_GRAB).await;
    assert_eq!(
        after.calibrated_value, None,
        "an uncorrected grab stays uncorrected"
    );
    assert_eq!(after.calibration_id, None, "and claims no calibration");
}

/// Deleting the calibration a reading was corrected with leaves it uncorrected. Writing the raw
/// value into `calibrated_value` instead would say a curve had been applied, which is the one thing
/// a null is there to distinguish.
#[tokio::test]
#[serial]
async fn deleting_the_only_covering_curve_leaves_the_value_null() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Plate-reader-04", GLOBAL_PARAM_TEMP_ID).await;

    let (status, body) = post_grab(
        &fx,
        json!({
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "sensor_id": sensor.id,
            "value": 10.0,
            "time": LATE_GRAB,
        }),
    )
    .await;
    assert_eq!(status, 200, "grab should succeed: {body}");
    assert_eq!(
        grab_at(&fx.db, LATE_GRAB).await.calibration_id,
        Some(sensor.base_calibration_id)
    );

    let (status, body) = delete_with_token(
        &fx.app,
        &format!("/api/sensor_calibrations/{}", sensor.base_calibration_id),
        &fx.token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "a windowed calibration is deletable: {body}"
    );

    let after = grab_at(&fx.db, LATE_GRAB).await;
    assert_eq!(
        after.calibration_id, None,
        "no remaining curve covers the reading"
    );
    assert_eq!(
        after.calibrated_value, None,
        "so it reads as uncorrected rather than as corrected by a curve nobody entered"
    );
}

/// An accepted update is where the provenance is recorded: an end date the operator sent is theirs
/// until they clear it, and clearing it hands the window back to the chain.
#[tokio::test]
#[serial]
async fn an_accepted_end_date_is_recorded_as_the_operators_and_giving_it_back_is_too() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Sonde-12", GLOBAL_PARAM_TEMP_ID).await;
    let curve = add_calibration(&fx.db, sensor.id, 2.0, 0.0, dt(CURVE_FROM)).await;

    let (status, body) =
        edit_calibration(&fx, curve, json!({ "valid_until": "2025-09-01T00:00:00Z" })).await;
    assert_eq!(status, 200, "retiring a curve is allowed: {body}");
    assert!(
        window_provenance(&fx.db, curve).await,
        "the end date the operator sent is theirs"
    );

    let (status, body) = edit_calibration(&fx, curve, json!({ "valid_until": null })).await;
    assert_eq!(status, 200, "clearing the end date is allowed: {body}");
    assert!(
        !window_provenance(&fx.db, curve).await,
        "and the chain has the window back"
    );
}

/// `valid_until_explicit` decides whether the window chain may reclaim a curve's end date. A
/// request the API rejects must not have changed it: the row would then be treated as
/// operator-ended forever, and deleting the following curve could no longer reopen the window.
#[tokio::test]
#[serial]
async fn a_rejected_update_leaves_the_window_provenance_unchanged() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Sonde-11", GLOBAL_PARAM_TEMP_ID).await;
    add_calibration(&fx.db, sensor.id, 2.0, 0.0, dt(CURVE_FROM)).await;
    let second = add_calibration(&fx.db, sensor.id, 3.0, 0.0, dt("2025-03-01T00:00:00Z")).await;

    let (status, body) = edit_calibration(
        &fx,
        second,
        json!({
            "valid_from": CURVE_FROM,
            "valid_until": "2025-06-01T00:00:00Z",
        }),
    )
    .await;
    assert_eq!(
        status, 400,
        "moving a curve onto another curve's opening instant is refused: {body}"
    );

    let row = fx
        .db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT valid_until_explicit, valid_from FROM sensor_calibrations WHERE id = '{second}'"
            ),
        ))
        .await
        .expect("query calibration")
        .expect("the curve is still there");
    assert!(
        !row.try_get::<bool>("", "valid_until_explicit").unwrap(),
        "a refused request leaves the end date the chain's to write"
    );
    let valid_from: chrono::DateTime<chrono::FixedOffset> = row.try_get("", "valid_from").unwrap();
    assert_eq!(
        valid_from.to_utc(),
        dt("2025-03-01T00:00:00Z"),
        "and leaves the start date where it was"
    );
}

/// What the spot recomposition does with a grab that names neither curve turns on what the stored
/// value is.
///
/// A copy of the raw value is what the old writers materialised for an uncorrected reading: it says
/// nothing the raw column does not, and the recomposition clears it. A different number is somebody
/// else's correction, arrived by a caller that supplied it without provenance, and nothing here can
/// reproduce it: it is reported by `/actions/calibration_candidates` and left standing.
#[tokio::test]
#[serial]
async fn a_grab_naming_no_curve_loses_a_copy_of_its_raw_value_but_keeps_a_correction() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Plate-reader-09", GLOBAL_PARAM_TEMP_ID).await;
    delete_calibration(&fx.db, sensor.base_calibration_id).await;

    let spot = |time: &str, calibrated: f64| {
        json!({
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "time": time,
            "raw_value": 10.0,
            "calibrated_value": calibrated,
            "sensor_id": sensor.id,
            "measurement_type": "spot",
        })
    };
    let (status, body) = post_json_with_token(
        &fx.app,
        "/api/readings/batch",
        &json!({ "readings": [spot(EARLY_GRAB, 10.0), spot(LATE_GRAB, 99.0)] }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "the batch insert should succeed: {body}");

    for (time, stored) in [(EARLY_GRAB, 10.0), (LATE_GRAB, 99.0)] {
        let entered = grab_at(&fx.db, time).await;
        assert_eq!(entered.calibration_id, None);
        assert_eq!(entered.standard_curve_id, None);
        assert_eq!(
            entered.calibrated_value,
            Some(stored),
            "the writer's value is what is stored, so the test starts from the state it creates"
        );
    }

    let (status, body) = post_json_with_token(
        &fx.app,
        "/api/actions/reprocess",
        &json!({ "sensor_id": sensor.id }),
        &fx.token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "reprocessing the instrument should be accepted: {body}"
    );
    assert!(
        wait_for_reprocessing(&fx.db, sensor.id, WAIT).await,
        "the reprocess job settles without failing"
    );

    let copied = grab_at(&fx.db, EARLY_GRAB).await;
    assert_eq!(
        copied.calibrated_value, None,
        "the copy of the raw value goes, so a null says plainly that no correction was made"
    );
    assert_eq!(copied.calibration_id, None, "and no curve is acquired");

    let corrected = grab_at(&fx.db, LATE_GRAB).await;
    assert_eq!(
        corrected.calibrated_value,
        Some(99.0),
        "the correction nothing here can reproduce is left exactly as the caller wrote it"
    );
    assert_eq!(corrected.calibration_id, None, "and stays unaccounted for");
    assert_eq!(corrected.standard_curve_id, None);
}

/// The same rule through the (site, parameter) engine, which is the one a backdate drives.
#[tokio::test]
#[serial]
async fn a_backdate_leaves_a_corrected_value_no_curve_accounts_for_standing() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Plate-reader-10", GLOBAL_PARAM_TEMP_ID).await;
    // The backdate walks the deployed slots, so the instrument has to occupy one.
    deploy_sensor(&fx.db, sensor.id, SITE1_ID, dt(DEPLOY_FROM)).await;
    delete_calibration(&fx.db, sensor.base_calibration_id).await;

    let (status, body) = post_json_with_token(
        &fx.app,
        "/api/readings/batch",
        &json!({
            "readings": [{
                "site_id": SITE1_ID,
                "parameter_id": GLOBAL_PARAM_TEMP_ID,
                "time": LATE_GRAB,
                "raw_value": 10.0,
                "calibrated_value": 99.0,
                "sensor_id": sensor.id,
                "measurement_type": "spot",
            }],
        }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "the batch insert should succeed: {body}");

    let (status, run) =
        post_json_parse_with_token(&fx.app, "/api/actions/reprocess_all", &json!({}), &fx.token)
            .await;
    assert!(
        (200..300).contains(&status),
        "the backdate should be accepted: {run}"
    );
    let job_id = run["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the backdate returns a tracked job: {run}"))
        .to_string();
    assert_eq!(
        e2e::poll_job(&fx.app, &fx.token, &job_id, 60).await,
        "completed",
        "the backdate runs to completion"
    );

    let after = grab_at(&fx.db, LATE_GRAB).await;
    assert_eq!(
        after.calibrated_value,
        Some(99.0),
        "the backdate reports a correction no curve accounts for, it does not overwrite one"
    );
    assert_eq!(after.calibration_id, None);
}
