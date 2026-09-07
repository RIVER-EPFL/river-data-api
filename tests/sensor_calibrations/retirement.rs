//! Retiring a windowed calibration, and putting it back.
//!
//! Scenario: a curve that corrected readings is taken out of circulation. Expected behaviour: the
//! readings move onto whatever else covers them and their values follow, the curve stays with
//! `retired_at` set and is never resolved again, each moved reading carries a `curve_retire`
//! decision naming the curve it left, and the retirement is one set that can be inverted.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use crate::common::sensor_lifecycle::{
    add_calibration_for_parameter, create_paired_stream, create_sensor_without_curve,
    deploy_sensor, dt, insert_readings,
};
use crate::common::{GLOBAL_PARAM_DO_ID, PARAM_S1_DO_ID, SITE1_ID};

const EARLY: &str = "2025-02-01T00:00:00Z";
const LATE: &str = "2025-08-01T00:00:00Z";

async fn value_at(db: &DatabaseConnection, time: &str) -> (Option<Uuid>, Option<f64>) {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT calibration_id, calibrated_value FROM readings \
                  WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}' \
                    AND time = '{time}'"
            ),
        ))
        .await
        .unwrap()
        .expect("the reading is stored");
    (
        row.try_get("", "calibration_id").unwrap(),
        row.try_get("", "calibrated_value").unwrap(),
    )
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .expect("a row")
    .try_get_by_index::<i64>(0)
    .expect("a count")
}

/// One instrument whose curves are exactly the two named here: an early one covering February and
/// a later one covering August, with nothing underneath them.
async fn two_curves(db: &DatabaseConnection) -> (Uuid, Uuid, Uuid) {
    let sensor_id = create_sensor_without_curve(db, "Retire-probe-01").await;
    let early = add_calibration_for_parameter(
        db,
        sensor_id,
        GLOBAL_PARAM_DO_ID,
        2.0,
        0.0,
        dt("2025-01-01T00:00:00Z"),
    )
    .await;
    let late = add_calibration_for_parameter(
        db,
        sensor_id,
        GLOBAL_PARAM_DO_ID,
        10.0,
        0.0,
        dt("2025-07-01T00:00:00Z"),
    )
    .await;
    let sensor = TestSensorId(sensor_id);
    let deployment = deploy_sensor(db, sensor.0, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    let stream = create_paired_stream(db, "retire-stream", PARAM_S1_DO_ID).await;
    insert_readings(
        db,
        stream,
        SITE1_ID,
        GLOBAL_PARAM_DO_ID,
        sensor.0,
        early,
        deployment,
        2.0,
        0.0,
        &[(dt(EARLY), 5.0)],
    )
    .await;
    insert_readings(
        db,
        stream,
        SITE1_ID,
        GLOBAL_PARAM_DO_ID,
        sensor.0,
        late,
        deployment,
        10.0,
        0.0,
        &[(dt(LATE), 5.0)],
    )
    .await;
    (sensor.0, early, late)
}

/// The sensor id alone: these tests build the curve timeline themselves rather than taking the
/// helper's base curve, which would cover every instant and hide what a retirement uncovers.
struct TestSensorId(Uuid);

#[tokio::test]
#[serial]
async fn retiring_a_curve_moves_its_readings_and_the_set_puts_them_back() {
    let f = crate::common::seeded_app().await;
    let (db, app, token) = (f.db, f.app, f.token);
    let (_sensor, early, late) = two_curves(&db).await;

    // The late curve opens on 2025-07-01, so the February reading is the early curve's alone.
    assert_eq!(value_at(&db, EARLY).await, (Some(early), Some(10.0)));

    let (status, report) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/sensor_calibrations/{early}/retire"),
        &serde_json::json!({ "dry_run": true }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "dry run: {report}");
    assert_eq!(report["readings"], 1, "one reading names the early curve");
    assert_eq!(
        report["uncorrected"], 1,
        "nothing else covers February, so the reading is left uncorrected: {report}"
    );
    assert!(
        report["retired_at"].is_null(),
        "a dry run changes nothing: {report}"
    );

    let (status, done) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/sensor_calibrations/{early}/retire"),
        &serde_json::json!({ "reason": "plate refitted" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "retire: {done}");
    let set_id = done["set_id"].as_str().expect("a decision set").to_string();

    // The row stays, stamped, and the reading it corrected no longer names it or carries its value.
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*)::bigint FROM sensor_calibrations \
                  WHERE id = '{early}' AND retired_at IS NOT NULL \
                    AND retired_reason = 'plate refitted'"
            )
        )
        .await,
        1,
        "the curve is retired, not deleted"
    );
    assert_eq!(
        value_at(&db, EARLY).await,
        (None, None),
        "no remaining curve covers February, so the reading is uncorrected"
    );
    assert_eq!(
        value_at(&db, LATE).await,
        (Some(late), Some(50.0)),
        "the later curve's readings are untouched"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*)::bigint FROM reading_decisions \
                  WHERE set_id = '{set_id}' AND kind = 'curve_retire' \
                    AND old ->> 'calibration_id' = '{early}'"
            )
        )
        .await,
        1,
        "the move is one decision per reading, naming the curve it left"
    );

    let (status, back) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/sensor_calibrations/{early}/unretire"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "unretire: {back}");
    assert_eq!(back["restored"], 1, "the set is inverted: {back}");
    assert_eq!(
        value_at(&db, EARLY).await,
        (Some(early), Some(10.0)),
        "the reading is back on the curve it was corrected by, at the value it produced"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*)::bigint FROM sensor_calibrations \
                  WHERE id = '{early}' AND retired_at IS NULL"
            )
        )
        .await,
        1,
        "the curve is offered again"
    );
}

#[tokio::test]
#[serial]
async fn a_curve_that_corrected_a_reading_is_not_deleted() {
    let f = crate::common::seeded_app().await;
    let (db, app, token) = (f.db, f.app, f.token);
    let (_sensor, early, _late) = two_curves(&db).await;

    let (status, body) = crate::common::delete_with_token(
        &app,
        &format!("/api/sensor_calibrations/{early}"),
        &token,
    )
    .await;
    assert_eq!(status, 400, "a used curve is retired, not deleted: {body}");
    assert!(body.contains("retire"), "the refusal says what to do: {body}");
    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*)::bigint FROM sensor_calibrations WHERE id = '{early}'")
        )
        .await,
        1,
        "the row is still there"
    );
}
