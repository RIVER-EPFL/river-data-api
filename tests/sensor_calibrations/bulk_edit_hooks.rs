//! `PATCH /sensor_calibrations/batch` runs the same rules as the one-row route.
//!
//! Scenario: an operator corrects several curves in one request.
//! Expected behaviour: each row meets the checks the single-row path applies, and each edit reaches
//! the readings it corrected, because a coefficient that moved silently is a wrong served value.

use sea_orm::DatabaseConnection;
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::sensor_lifecycle::{add_calibration, create_sensor, deploy_sensor, dt};
use crate::common::{GLOBAL_PARAM_DO_ID, SITE1_ID};

async fn setup() -> (DatabaseConnection, axum::Router, String, Uuid, Uuid) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor = create_sensor(&db, "Bulk-probe-01", GLOBAL_PARAM_DO_ID).await;
    let first = add_calibration(&db, sensor.id, 2.0, 1.0, dt("2025-01-01T00:00:00Z")).await;
    let second = add_calibration(&db, sensor.id, 3.0, 0.0, dt("2025-03-01T00:00:00Z")).await;
    deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    (db, app, token, first, second)
}

async fn patch_batch(app: &axum::Router, token: &str, body: &serde_json::Value) -> (u16, String) {
    crate::common::patch_json_with_token(app, "/api/sensor_calibrations/batch", body, token).await
}

async fn jobs_of_type(db: &DatabaseConnection, trigger: &str) -> i64 {
    crate::common::e2e::count(
        db,
        &format!("SELECT count(*) AS c FROM reprocessing_jobs WHERE trigger_type = '{trigger}'"),
    )
    .await
}

#[tokio::test]
#[serial]
async fn a_bulk_coefficient_edit_reaches_the_readings_it_corrected() {
    let (db, app, token, first, second) = setup().await;
    let before = jobs_of_type(&db, "calibration_update").await;

    let (status, body) = patch_batch(
        &app,
        &token,
        &json!([
            { "id": first, "slope": 5.0 },
            { "id": second, "slope": 7.0 },
        ]),
    )
    .await;
    assert_eq!(status, 200, "a bulk edit of two curves ({status}): {body}");

    assert_eq!(
        jobs_of_type(&db, "calibration_update").await - before,
        2,
        "each edited curve enqueues the reprocess its readings need"
    );
}

#[tokio::test]
#[serial]
async fn a_bulk_edit_meets_the_same_checks_as_one_row() {
    let (db, app, token, first, _) = setup().await;
    let before = jobs_of_type(&db, "calibration_update").await;

    let (status, body) = patch_batch(&app, &token, &json!([{ "id": first, "slope": 0.0 }])).await;
    assert_eq!(
        status, 400,
        "a zero slope is refused in a batch too ({status}): {body}"
    );
    assert_eq!(
        jobs_of_type(&db, "calibration_update").await,
        before,
        "and a refused edit enqueues nothing"
    );
}
