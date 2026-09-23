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
    db.query_one_raw(Statement::from_string(
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
    assert!(
        (raw - 10.0).abs() < 1e-9,
        "the measured value is kept as raw"
    );
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
    assert_eq!(
        status, 400,
        "an unknown curve id is refused ({status}): {body}"
    );

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

/// An overwrite replaces the measurement, not the correction: the row keeps its recorded curves,
/// so the stored value is recomputed from them rather than left as whatever the overwrite carried.
#[tokio::test]
#[serial]
async fn an_overwrite_recomposes_the_value_from_the_rows_own_curves() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Batch-Plate-02", GLOBAL_PARAM_TEMP_ID).await;
    let curve = create_curve(&fx, sensor.id, "Batch Plate B", 2.0, 1.0).await;

    let (status, body) = post_batch(
        &fx,
        json!({
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "time": GRAB_TIME,
            "raw_value": 10.0,
            "sensor_id": sensor.id,
            "standard_curve_id": curve,
            "measurement_type": "spot",
        }),
    )
    .await;
    assert_eq!(status, 200, "initial insert ({status}): {body}");

    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        "/api/readings/batch",
        &json!({
            "conflict": "overwrite",
            "readings": [{
                "site_id": SITE1_ID,
                "parameter_id": GLOBAL_PARAM_TEMP_ID,
                "time": GRAB_TIME,
                "raw_value": 20.0,
                "measurement_type": "spot",
            }],
        }),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "overwrite ({status}): {body}");

    let (raw, calibrated, stored_curve) = stored_at(&fx.db, GRAB_TIME)
        .await
        .expect("the reading is stored");
    assert!(
        (raw - 20.0).abs() < 1e-9,
        "the correction replaced the raw value"
    );
    assert_eq!(
        stored_curve,
        Some(curve),
        "the hand-picked curve outlives a correction that names none"
    );
    assert!(
        (calibrated.expect("the row still names a curve") - 41.0).abs() < 1e-9,
        "recomposed from the row's own curve: 2.0 * 20.0 + 1.0"
    );
}

/// A stamped calibration is a curve the stored value went through: a row whose caller supplied no
/// value gets its named base applied rather than a NULL beside a claimed correction.
#[tokio::test]
#[serial]
async fn a_named_base_calibration_is_applied_not_just_stamped() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Batch-Plate-03", GLOBAL_PARAM_TEMP_ID).await;
    let calibration = crate::common::sensor_lifecycle::add_calibration(
        &fx.db,
        sensor.id,
        3.0,
        2.0,
        crate::common::sensor_lifecycle::dt("2025-01-01T00:00:00Z"),
    )
    .await;

    let (status, body) = post_batch(
        &fx,
        json!({
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "time": GRAB_TIME,
            "raw_value": 10.0,
            "sensor_id": sensor.id,
            "calibration_id": calibration,
        }),
    )
    .await;
    assert_eq!(status, 200, "batch with a named base ({status}): {body}");

    let row = fx
        .db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT calibrated_value, calibration_id FROM readings \
                 WHERE parameter_id = '{GLOBAL_PARAM_TEMP_ID}' AND time = '{GRAB_TIME}'"
            ),
        ))
        .await
        .unwrap()
        .expect("the reading is stored");
    assert_eq!(
        row.try_get::<Option<Uuid>>("", "calibration_id").unwrap(),
        Some(calibration)
    );
    let calibrated = row
        .try_get::<Option<f64>>("", "calibrated_value")
        .unwrap()
        .expect("the named base is applied");
    assert!(
        (calibrated - 32.0).abs() < 1e-9,
        "3.0 * 10.0 + 2.0: got {calibrated}"
    );
}

async fn stored_correction(db: &DatabaseConnection) -> Option<(Option<f64>, Option<Uuid>)> {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT calibrated_value, calibration_id FROM readings \
             WHERE parameter_id = '{GLOBAL_PARAM_TEMP_ID}' AND time = '{GRAB_TIME}'"
        ),
    ))
    .await
    .unwrap()
    .map(|row| {
        (
            row.try_get("", "calibrated_value").unwrap(),
            row.try_get("", "calibration_id").unwrap(),
        )
    })
}

/// A submitted value the row's calibration does not produce is refused, whether the row names the
/// calibration or inherits it from the instrument deployed in the slot, since storing it would
/// record a curve that did not make the number and the drift sweep would later rewrite it.
#[tokio::test]
#[serial]
async fn a_submitted_value_its_calibration_does_not_produce_is_refused() {
    let fx = setup().await;
    let sensor = create_sensor(&fx.db, "Batch-Plate-04", GLOBAL_PARAM_TEMP_ID).await;
    crate::common::sensor_lifecycle::deploy_sensor_for_parameter(
        &fx.db,
        sensor.id,
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        crate::common::sensor_lifecycle::dt("2025-01-01T00:00:00Z"),
    )
    .await;
    let calibration = crate::common::sensor_lifecycle::add_calibration(
        &fx.db,
        sensor.id,
        2.0,
        0.0,
        crate::common::sensor_lifecycle::dt("2025-01-01T00:00:00Z"),
    )
    .await;

    let named = json!({
        "site_id": SITE1_ID,
        "parameter_id": GLOBAL_PARAM_TEMP_ID,
        "time": GRAB_TIME,
        "raw_value": 10.0,
        "calibrated_value": 99.0,
        "sensor_id": sensor.id,
        "calibration_id": calibration,
    });
    let inherited = json!({
        "site_id": SITE1_ID,
        "parameter_id": GLOBAL_PARAM_TEMP_ID,
        "time": GRAB_TIME,
        "raw_value": 10.0,
        "calibrated_value": 99.0,
    });
    for reading in [named, inherited] {
        let (status, body) = post_batch(&fx, reading.clone()).await;
        assert_eq!(status, 400, "{reading} ({status}): {body}");
        assert!(
            body.contains(&calibration.to_string()),
            "the refusal names the calibration: {body}"
        );
        assert_eq!(stored_correction(&fx.db).await, None, "nothing is stored");
    }

    let (status, body) = post_batch(
        &fx,
        json!({
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "time": GRAB_TIME,
            "raw_value": 10.0,
            "calibrated_value": 20.0,
        }),
    )
    .await;
    assert_eq!(
        status, 200,
        "a value the calibration produces ({status}): {body}"
    );
    assert_eq!(
        stored_correction(&fx.db).await,
        // 2.0 * 10.0 + 0.0
        Some((Some(20.0), Some(calibration)))
    );
}

/// Scenario: a low-frequency instrument is deployed at a site and parameter the site carries no
/// slot for, and batch rows there name a curve fitted on it.
/// Expected behaviour: the rows land on an unpaired channel and store only the instrument they
/// declare, so a row naming none has its curve refused and is not classified by the deployed
/// instrument's cadence, and a row declaring the instrument is judged against it.
#[tokio::test]
#[serial]
async fn a_row_on_an_unpaired_channel_is_judged_by_the_instrument_it_declares() {
    use crate::common::sensor_lifecycle::{
        create_sensor_without_curve, deploy_sensor_for_parameter, dt,
    };
    let fx = setup().await;
    let param =
        crate::common::e2e::create_parameter(&fx.app, &fx.token, "noslotcurve", "No slot", "m")
            .await;
    let sensor = create_sensor_without_curve(&fx.db, "Batch-Plate-Unslotted").await;
    fx.db
        .execute_unprepared(&format!(
            "UPDATE sensors SET data_frequency = 'low' WHERE id = '{sensor}'"
        ))
        .await
        .expect("mark the instrument low-frequency");
    deploy_sensor_for_parameter(&fx.db, sensor, SITE1_ID, &param, dt("2025-01-01T00:00:00Z")).await;
    let curve = create_curve(&fx, sensor, "Batch Plate Unslotted", 2.0, 1.0).await;
    let row = |time: &str| {
        json!({
            "site_id": SITE1_ID,
            "parameter_id": param,
            "time": time,
            "raw_value": 10.0,
        })
    };
    let stored = async |time: &str| {
        fx.db
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                format!(
                    "SELECT r.measurement_type, r.sensor_id, r.standard_curve_id \
                     FROM readings r JOIN data_streams s ON s.id = r.stream_id \
                     WHERE s.source_system = 'api' \
                     AND s.source_key = '{SITE1_ID}:{param}' AND r.time = '{time}'"
                ),
            ))
            .await
            .expect("query readings")
            .map(|r| {
                (
                    r.try_get::<Option<String>>("", "measurement_type").unwrap(),
                    r.try_get::<Option<Uuid>>("", "sensor_id").unwrap(),
                    r.try_get::<Option<Uuid>>("", "standard_curve_id").unwrap(),
                )
            })
    };

    let mut claimed = row("2025-03-01T00:00:00Z");
    claimed["standard_curve_id"] = json!(curve);
    let (status, body) = post_batch(&fx, claimed).await;
    assert_eq!(
        status, 400,
        "a curve on a row naming no instrument is refused ({status}): {body}"
    );

    let (status, body) = post_batch(&fx, row("2025-03-02T00:00:00Z")).await;
    assert_eq!(status, 200, "a plain row is staged ({status}): {body}");
    assert_eq!(
        stored("2025-03-02T00:00:00Z").await,
        Some((Some("continuous".to_string()), None, None)),
        "the deployed instrument's cadence does not classify a row that does not name it"
    );

    let mut declared = row("2025-03-03T00:00:00Z");
    declared["sensor_id"] = json!(sensor);
    declared["standard_curve_id"] = json!(curve);
    let (status, body) = post_batch(&fx, declared).await;
    assert_eq!(
        status, 200,
        "a row declaring the curve's instrument is admitted ({status}): {body}"
    );
    assert_eq!(
        stored("2025-03-03T00:00:00Z").await,
        Some((Some("spot".to_string()), Some(sensor), Some(curve))),
    );
}
