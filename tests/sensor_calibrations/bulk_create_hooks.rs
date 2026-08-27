//! `POST /sensor_calibrations/batch` and `POST /standard_curves/batch` run the same rules as the
//! one-row route.
//!
//! Scenario: an operator enters several curves in one request.
//! Expected behaviour: the batch succeeds through the single-row path (the crudcrate default
//! `create_many` recursed between the resource and the operations until the stack overflowed) and
//! each row meets the checks the single-row path applies.

use sea_orm::DatabaseConnection;
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::GLOBAL_PARAM_DO_ID;
use crate::common::sensor_lifecycle::create_sensor_without_curve;

async fn setup() -> (DatabaseConnection, axum::Router, String, Uuid) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor = create_sensor_without_curve(&db, "Bulk-create-probe-01").await;
    (db, app, token, sensor)
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    crate::common::e2e::count(db, sql).await
}

#[tokio::test]
#[serial]
async fn a_calibration_batch_is_created_through_the_single_row_path() {
    let (db, app, token, sensor) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/sensor_calibrations/batch",
        &json!([
            {
                "sensor_id": sensor,
                "parameter_id": GLOBAL_PARAM_DO_ID,
                "slope": 2.0,
                "intercept": 1.0,
                "valid_from": "2025-01-01T00:00:00Z",
            },
            {
                "sensor_id": sensor,
                "parameter_id": GLOBAL_PARAM_DO_ID,
                "slope": 3.0,
                "intercept": 0.0,
                "valid_from": "2025-03-01T00:00:00Z",
            },
        ]),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "a batch of two curves is created ({status}): {body}"
    );

    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) AS c FROM sensor_calibrations WHERE sensor_id = '{sensor}'")
        )
        .await,
        2,
        "both rows are stored"
    );
}

#[tokio::test]
#[serial]
async fn a_calibration_batch_meets_the_same_checks_as_one_row() {
    let (db, app, token, sensor) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/sensor_calibrations/batch",
        &json!([{
            "sensor_id": sensor,
            "parameter_id": GLOBAL_PARAM_DO_ID,
            "slope": 0.0,
            "intercept": 1.0,
            "valid_from": "2025-01-01T00:00:00Z",
        }]),
        &token,
    )
    .await;
    assert_eq!(
        status, 400,
        "a zero slope is refused in a batch too ({status}): {body}"
    );

    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) AS c FROM sensor_calibrations WHERE sensor_id = '{sensor}'")
        )
        .await,
        0,
        "a refused batch stores nothing"
    );
}

#[tokio::test]
#[serial]
async fn a_standard_curve_batch_is_created_through_the_single_row_path() {
    let (db, app, token, sensor) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/standard_curves/batch",
        &json!([
            { "sensor_id": sensor, "name": "plate A", "slope": 1.1, "intercept": 0.2 },
            { "sensor_id": sensor, "name": "plate B", "slope": 0.9, "intercept": -0.1 },
        ]),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "a batch of two standard curves is created ({status}): {body}"
    );

    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) AS c FROM standard_curves WHERE sensor_id = '{sensor}'")
        )
        .await,
        2,
        "both curves are stored"
    );
}

#[tokio::test]
#[serial]
async fn a_standard_curve_batch_meets_the_same_checks_as_one_row() {
    let (db, app, token, sensor) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/standard_curves/batch",
        &json!([{ "sensor_id": sensor, "name": "flat plate", "slope": 0.0, "intercept": 0.2 }]),
        &token,
    )
    .await;
    assert_eq!(
        status, 400,
        "a zero slope is refused in a batch too ({status}): {body}"
    );

    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) AS c FROM standard_curves WHERE sensor_id = '{sensor}'")
        )
        .await,
        0,
        "a refused batch stores nothing"
    );
}
