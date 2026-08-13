//! `POST /readings/batch` accepting a hand-picked standard curve on a reading.
//!
//! Scenario: a caller replays lab grabs through the batch endpoint, each row naming the curve it
//! was measured against.
//! Expected behaviour: a row may name a curve only when it is that instrument's own spot
//! measurement, and the stored corrected value is computed from the curve rather than taken from
//! the request, so the reference and the value it claims to explain cannot disagree.
//!
//! Run: cargo test --test readings batch_standard_curve -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::sensor_lifecycle::create_sensor;
use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const GRAB_TIME: &str = "2025-06-15T10:00:00Z";

struct Fixture {
    app: axum::Router,
    db: DatabaseConnection,
    token: String,
}

async fn setup() -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    Fixture { app, db, token }
}

async fn create_curve(
    fx: &Fixture,
    sensor_id: Uuid,
    name: &str,
    slope: f64,
    intercept: f64,
) -> Uuid {
    let (status, body) = crate::common::post_json_parse_with_token(
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
        "creating standard curve '{name}' ({status}): {body}"
    );
    body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("created curve carries no id: {body}"))
        .parse()
        .expect("curve id is a uuid")
}

async fn post_batch(fx: &Fixture, reading: serde_json::Value) -> (u16, String) {
    crate::common::post_json_with_token(
        &fx.app,
        "/api/readings/batch",
        &json!({ "readings": [reading] }),
        &fx.token,
    )
    .await
}

async fn stored_at(
    db: &DatabaseConnection,
    time: &str,
) -> Option<(f64, Option<f64>, Option<Uuid>)> {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT raw_value, calibrated_value, standard_curve_id FROM readings \
             WHERE parameter_id = '{GLOBAL_PARAM_TEMP_ID}' AND time = '{time}'"
        ),
    ))
    .await
    .expect("query readings")
    .map(|row| {
        (
            row.try_get("", "raw_value").unwrap(),
            row.try_get("", "calibrated_value").unwrap(),
            row.try_get("", "standard_curve_id").unwrap(),
        )
    })
}

#[tokio::test]
#[serial]
async fn a_spot_row_on_the_curves_instrument_is_stored_with_the_value_the_curve_produces() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Batch-Plate-01", GLOBAL_PARAM_TEMP_ID).await;
    let curve = create_curve(&fx, sensor.id, "Batch Plate A", 2.0, 1.0).await;

    let (status, body) = post_batch(
        &fx,
        json!({
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "time": GRAB_TIME,
            "raw_value": 10.0,
            "calibrated_value": 999.0,
            "sensor_id": sensor.id,
            "standard_curve_id": curve,
            "measurement_type": "spot",
        }),
    )
    .await;
    assert_eq!(status, 200, "batch with a curve ({status}): {body}");

    let (raw, calibrated, stored_curve) = stored_at(&fx.db, GRAB_TIME)
        .await
        .expect("the reading is stored");
    assert!((raw - 10.0).abs() < 1e-9, "the measured value is kept as raw");
    assert_eq!(stored_curve, Some(curve), "the curve is recorded");
    let calibrated = calibrated.expect("a curve applied means a value");
    assert!(
        (calibrated - 21.0).abs() < 1e-9,
        "the curve produces 2.0 * 10.0 + 1.0, not the submitted 999: got {calibrated}"
    );
}

#[tokio::test]
#[serial]
async fn a_curve_from_another_instrument_or_from_nowhere_is_refused() {
    let fx = setup().await;
    let plate_reader = create_sensor(&fx.db, "Batch-Plate-02", GLOBAL_PARAM_TEMP_ID).await;
    let other = create_sensor(&fx.db, "Batch-Plate-03", GLOBAL_PARAM_TEMP_ID).await;
    let curve = create_curve(&fx, other.id, "Batch Plate B", 2.0, 1.0).await;

    let row = |curve_id: Uuid, sensor: Option<Uuid>| {
        let mut row = json!({
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "time": GRAB_TIME,
            "raw_value": 10.0,
            "standard_curve_id": curve_id,
            "measurement_type": "spot",
        });
        if let Some(sensor) = sensor {
            row["sensor_id"] = json!(sensor);
        }
        row
    };

    let (status, body) = post_batch(&fx, row(curve, Some(plate_reader.id))).await;
    assert_eq!(
        status, 400,
        "a curve fitted on another instrument is refused ({status}): {body}"
    );

    let (status, body) = post_batch(&fx, row(Uuid::new_v4(), Some(plate_reader.id))).await;
    assert_eq!(status, 400, "an unknown curve id is refused ({status}): {body}");

    let (status, body) = post_batch(&fx, row(curve, None)).await;
    assert_eq!(
        status, 400,
        "a row naming a curve but no instrument is refused ({status}): {body}"
    );

    assert!(
        stored_at(&fx.db, GRAB_TIME).await.is_none(),
        "a refused batch stores nothing"
    );
}

/// A curve is fitted for one hand-picked measurement, so a logger series has nothing to pick it
/// for. Accepting one there would freeze the curve against edits and name it on values reprocessing
/// recomputes from the instrument's windows alone.
#[tokio::test]
#[serial]
async fn a_continuous_row_cannot_name_a_standard_curve() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Batch-Plate-04", GLOBAL_PARAM_TEMP_ID).await;
    let curve = create_curve(&fx, sensor.id, "Batch Plate C", 2.0, 1.0).await;

    let (status, body) = post_batch(
        &fx,
        json!({
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "time": GRAB_TIME,
            "raw_value": 10.0,
            "calibrated_value": 999.0,
            "sensor_id": sensor.id,
            "standard_curve_id": curve,
            "measurement_type": "continuous",
        }),
    )
    .await;
    assert_eq!(
        status, 400,
        "a continuous row naming a curve is refused ({status}): {body}"
    );
    assert!(
        stored_at(&fx.db, GRAB_TIME).await.is_none(),
        "a refused batch stores nothing"
    );
}
