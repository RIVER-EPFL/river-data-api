//! The janitor's curve-drift sweep: a corrected reading must serve what its own curves produce.
//!
//! Scenario: coefficients move by a route that runs no hook (a bulk edit, a direct statement, an
//! enqueue that never landed), so stored values are left computed from the old ones.
//! Expected behaviour: the sweep recomposes them from the curves each row names, both the windowed
//! calibration and the standard curve, and reports the span it moved so the rollups can follow.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use river_db::routes::private::sensors::calibrations::service::sweep_curve_drift;

use crate::common::sensor_lifecycle::{add_calibration, create_sensor, deploy_sensor, dt};
use crate::common::{GLOBAL_PARAM_DO_ID, SITE1_ID};

const GRAB_TIME: &str = "2025-06-15T10:00:00Z";

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

async fn stored(db: &DatabaseConnection) -> (f64, Option<f64>) {
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT raw_value, calibrated_value FROM readings \
                 WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}' \
                 AND time = '{GRAB_TIME}'"
            ),
        ))
        .await
        .unwrap()
        .expect("the grab is stored");
    (
        row.try_get("", "raw_value").unwrap(),
        row.try_get("", "calibrated_value").unwrap(),
    )
}

/// Deploy a lab instrument carrying a windowed calibration, so a grab against it resolves a base
/// curve and may also name a standard curve.
async fn deployed_lab_sensor(db: &DatabaseConnection, slope: f64, intercept: f64) -> (Uuid, Uuid) {
    let sensor = create_sensor(db, "Drift-probe-01", GLOBAL_PARAM_DO_ID).await;
    crate::common::exec(
        db,
        &format!(
            "UPDATE sensors SET is_lab_instrument = true WHERE id = '{}'",
            sensor.id
        ),
    )
    .await;
    let calibration =
        add_calibration(db, sensor.id, slope, intercept, dt("2025-01-01T00:00:00Z")).await;
    deploy_sensor(db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    (sensor.id, calibration)
}

async fn post_grab(
    app: &axum::Router,
    token: &str,
    sensor_id: Uuid,
    curve: Option<Uuid>,
    value: f64,
) {
    let mut reading = serde_json::json!({
        "parameter_id": GLOBAL_PARAM_DO_ID,
        "sensor_id": sensor_id,
        "value": value,
        "time": GRAB_TIME,
    });
    if let Some(curve) = curve {
        reading["standard_curve_id"] = serde_json::json!(curve);
    }
    let (status, body) = crate::common::post_json_with_token(
        app,
        "/api/grab_samples",
        &serde_json::json!({ "site_id": SITE1_ID, "readings": [reading] }),
        token,
    )
    .await;
    assert_eq!(status, 200, "grab entry: {body}");
}

#[tokio::test]
#[serial]
async fn the_sweep_recomposes_a_value_left_behind_by_an_unhooked_coefficient_edit() {
    let (db, app, token) = setup().await;
    let (sensor, calibration) = deployed_lab_sensor(&db, 2.0, 1.0).await;

    post_grab(&app, &token, sensor, None, 10.0).await;
    assert_eq!(stored(&db).await, (10.0, Some(21.0)), "2 * 10 + 1");

    crate::common::exec(
        &db,
        &format!("UPDATE sensor_calibrations SET slope = 5.0 WHERE id = '{calibration}'"),
    )
    .await;
    assert_eq!(
        stored(&db).await.1,
        Some(21.0),
        "the statement moved the curve and nothing recomputed the reading"
    );

    let drift = sweep_curve_drift(&db).await.expect("sweep runs");
    assert_eq!(drift.moved, 1, "the one drifted reading is recomposed");
    assert_eq!(stored(&db).await, (10.0, Some(51.0)), "5 * 10 + 1");
    assert!(
        drift.span.is_some(),
        "the sweep reports the span it moved, so the rollups can follow"
    );

    let second = sweep_curve_drift(&db).await.expect("sweep runs");
    assert_eq!(second.moved, 0, "a settled row is not rewritten again");
}

/// A grab may carry both curves, and the value is the standard curve applied on top of the base.
/// The sweep has to compose them in that order, not pick one.
#[tokio::test]
#[serial]
async fn the_sweep_composes_the_standard_curve_over_the_windowed_calibration() {
    let (db, app, token) = setup().await;
    let (sensor, calibration) = deployed_lab_sensor(&db, 2.0, 1.0).await;

    let curve = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept, name) \
             VALUES ('{curve}', '{sensor}', 3.0, 0.5, 'Plate A')"
        ),
    )
    .await;

    post_grab(&app, &token, sensor, Some(curve), 10.0).await;
    assert_eq!(
        stored(&db).await,
        (10.0, Some(63.5)),
        "3 * (2 * 10 + 1) + 0.5"
    );

    crate::common::exec(
        &db,
        &format!("UPDATE standard_curves SET slope = 4.0 WHERE id = '{curve}'"),
    )
    .await;
    let drift = sweep_curve_drift(&db).await.expect("sweep runs");
    assert_eq!(
        drift.moved, 1,
        "the lab curve moved, so the grab follows it"
    );
    assert_eq!(
        stored(&db).await,
        (10.0, Some(84.5)),
        "4 * (2 * 10 + 1) + 0.5: the base is still applied underneath"
    );

    crate::common::exec(
        &db,
        &format!("UPDATE sensor_calibrations SET slope = 5.0 WHERE id = '{calibration}'"),
    )
    .await;
    let drift = sweep_curve_drift(&db).await.expect("sweep runs");
    assert_eq!(drift.moved, 1, "and it follows the base curve too");
    assert_eq!(
        stored(&db).await,
        (10.0, Some(204.5)),
        "4 * (5 * 10 + 1) + 0.5"
    );
}

/// Broken data, repaired: the stored number is overwritten with one no curve produces, and the
/// sweep puts back exactly what the row's curves give.
#[tokio::test]
#[serial]
async fn the_sweep_repairs_a_value_corrupted_in_place() {
    let (db, app, token) = setup().await;
    let (sensor, _) = deployed_lab_sensor(&db, 2.0, 1.0).await;

    post_grab(&app, &token, sensor, None, 10.0).await;
    assert_eq!(stored(&db).await, (10.0, Some(21.0)), "2 * 10 + 1");

    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET calibrated_value = 12345.0 \
             WHERE time = '{GRAB_TIME}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}'"
        ),
    )
    .await;
    assert_eq!(stored(&db).await.1, Some(12345.0), "the row is now wrong");

    let drift = sweep_curve_drift(&db).await.expect("sweep runs");
    assert_eq!(drift.moved, 1);
    assert_eq!(
        stored(&db).await,
        (10.0, Some(21.0)),
        "the curve on the row decides the value, whatever was written over it"
    );
}

/// Clean data, untouched: every stored value already agrees with its curves, so the sweep reports
/// nothing moved and rewrites no row, including the uncorrected and the both-curves cases.
#[tokio::test]
#[serial]
async fn the_sweep_moves_nothing_on_clean_data() {
    let (db, app, token) = setup().await;
    let (sensor, _) = deployed_lab_sensor(&db, 2.0, 1.0).await;

    let curve = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept, name) \
             VALUES ('{curve}', '{sensor}', 3.0, 0.5, 'Plate A')"
        ),
    )
    .await;
    post_grab(&app, &token, sensor, Some(curve), 10.0).await;

    let before = crate::common::e2e::count(
        &db,
        &format!(
            "SELECT count(*) AS c FROM readings \
             WHERE site_id = '{SITE1_ID}' AND calibrated_value IS NOT NULL"
        ),
    )
    .await;
    let value_before = stored(&db).await;

    let drift = sweep_curve_drift(&db).await.expect("sweep runs");
    assert_eq!(
        drift.moved, 0,
        "nothing had drifted, so nothing is rewritten"
    );
    assert!(drift.span.is_none(), "and there is no span to refresh");
    assert_eq!(
        stored(&db).await,
        value_before,
        "the value is byte-identical afterwards"
    );
    assert_eq!(
        crate::common::e2e::count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings \
                 WHERE site_id = '{SITE1_ID}' AND calibrated_value IS NOT NULL"
            )
        )
        .await,
        before,
        "and no row gained or lost a correction"
    );
}

/// A corrected value no curve on the row accounts for was produced by a method this code cannot
/// recover, so the sweep reports nothing and leaves it exactly as it is.
#[tokio::test]
#[serial]
async fn the_sweep_leaves_a_correction_no_curve_accounts_for() {
    let (db, app, token) = setup().await;
    let (sensor, calibration) = deployed_lab_sensor(&db, 2.0, 1.0).await;

    post_grab(&app, &token, sensor, None, 10.0).await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET calibration_id = NULL, calibrated_value = 99.0 \
             WHERE time = '{GRAB_TIME}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}'"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!("UPDATE sensor_calibrations SET slope = 5.0 WHERE id = '{calibration}'"),
    )
    .await;

    let drift = sweep_curve_drift(&db).await.expect("sweep runs");
    assert_eq!(
        drift.moved, 0,
        "it names no curve, so there is nothing to recompose from"
    );
    assert_eq!(
        stored(&db).await.1,
        Some(99.0),
        "and the number is untouched"
    );
}
