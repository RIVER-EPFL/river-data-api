//! Splitting readings onto another instrument, and what happens to the correction they carry.
//!
//! A standard curve belongs to exactly one instrument, so a reading pinned to a different one
//! cannot keep pointing at the curve it was corrected with (Q112). The pin asks, and applies the
//! answer in its own transaction.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use crate::common::sensor_lifecycle::{add_calibration, create_sensor, deploy_sensor, dt};
use crate::common::{GLOBAL_PARAM_DO_ID, SITE1_ID};

const AT: &str = "2025-06-15T10:00:00Z";

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

/// A lab instrument with a base calibration. Only the instrument the readings start on takes the
/// deployment: one sensor holds a (site, parameter) slot at a time, and the incoming instrument
/// needs none, since a grab's slot comes from the pairing rather than from a deployment window.
async fn lab_sensor(db: &DatabaseConnection, name: &str, deployed: bool) -> Uuid {
    let sensor = create_sensor(db, name, GLOBAL_PARAM_DO_ID).await;
    crate::common::exec(
        db,
        &format!("UPDATE sensors SET is_lab_instrument = true WHERE id = '{}'", sensor.id),
    )
    .await;
    add_calibration(db, sensor.id, 2.0, 1.0, dt("2025-01-01T00:00:00Z")).await;
    if deployed {
        deploy_sensor(db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    }
    sensor.id
}

async fn curve_of(db: &DatabaseConnection) -> Option<Uuid> {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT standard_curve_id FROM readings \
             WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}' \
               AND time = '{AT}'"
        ),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get("", "standard_curve_id")
    .unwrap()
}

async fn stream_of(db: &DatabaseConnection) -> Uuid {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT stream_id FROM readings \
             WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}' \
               AND time = '{AT}'"
        ),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get("", "stream_id")
    .unwrap()
}

/// A grab corrected by `curve` on `sensor`, ready to be split onto another instrument.
async fn corrected_grab(
    db: &DatabaseConnection,
    app: &axum::Router,
    token: &str,
    sensor: Uuid,
) -> Uuid {
    let curve = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept, name) \
             VALUES ('{curve}', '{sensor}', 3.0, 0.5, 'Plate A')"
        ),
    )
    .await;
    let (status, body) = crate::common::post_json_with_token(
        app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": SITE1_ID,
            "created_by": "lab",
            "readings": [{
                "parameter_id": GLOBAL_PARAM_DO_ID,
                "value": 10.0,
                "time": AT,
                "sensor_id": sensor,
                "standard_curve_id": curve,
            }],
        }),
        token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    curve
}

async fn pin(
    app: &axum::Router,
    token: &str,
    stream: Uuid,
    target: Uuid,
    curves: Option<&str>,
) -> (u16, serde_json::Value) {
    let mut body = serde_json::json!({
        "kind": "instrument",
        "target_id": target,
        "selection": { "keys": [{ "stream_id": stream, "time": AT, "replicate_index": 0 }] },
        "reason": "measured on the other analyser",
    });
    if let Some(c) = curves {
        body["curves"] = serde_json::Value::String(c.to_string());
    }
    crate::common::post_json_parse_with_token(app, "/api/readings/pins", &body, token).await
}

#[tokio::test]
#[serial]
async fn a_split_that_does_not_say_what_happens_to_the_curve_is_refused() {
    let (db, app, token) = setup().await;
    let from = lab_sensor(&db, "Analyser A", true).await;
    let to = lab_sensor(&db, "Analyser B", false).await;
    let curve = corrected_grab(&db, &app, &token, from).await;
    let stream = stream_of(&db).await;

    let (status, body) = pin(&app, &token, stream, to, None).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        curve_of(&db).await,
        Some(curve),
        "a refused pin writes nothing at all"
    );
}

#[tokio::test]
#[serial]
async fn copying_the_curve_takes_the_correction_with_the_readings() {
    let (db, app, token) = setup().await;
    let from = lab_sensor(&db, "Analyser A", true).await;
    let to = lab_sensor(&db, "Analyser B", false).await;
    let original = corrected_grab(&db, &app, &token, from).await;
    let stream = stream_of(&db).await;

    let (status, body) = pin(&app, &token, stream, to, Some("copy")).await;
    assert_eq!(status, 200, "{body}");

    let now = curve_of(&db).await.expect("the reading still names a curve");
    assert_ne!(now, original, "it names the copy, not the original");
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT sensor_id, copied_from_id, slope, intercept FROM standard_curves WHERE id = '{now}'"),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.try_get::<Uuid>("", "sensor_id").unwrap(), to);
    assert_eq!(
        row.try_get::<Option<Uuid>>("", "copied_from_id").unwrap(),
        Some(original),
        "the copy says which curve it came from"
    );
    assert_eq!(row.try_get::<f64>("", "slope").unwrap(), 3.0);
    assert_eq!(row.try_get::<f64>("", "intercept").unwrap(), 0.5);
}

#[tokio::test]
#[serial]
async fn dropping_the_curve_leaves_the_readings_with_none() {
    let (db, app, token) = setup().await;
    let from = lab_sensor(&db, "Analyser A", true).await;
    let to = lab_sensor(&db, "Analyser B", false).await;
    corrected_grab(&db, &app, &token, from).await;
    let stream = stream_of(&db).await;

    let (status, body) = pin(&app, &token, stream, to, Some("drop")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        curve_of(&db).await,
        None,
        "no reading names a curve of an instrument it does not belong to"
    );
}

/// A pin onto the instrument that already owns the curve is not a split at all.
#[tokio::test]
#[serial]
async fn pinning_to_the_curve_s_own_instrument_needs_no_answer() {
    let (db, app, token) = setup().await;
    let from = lab_sensor(&db, "Analyser A", true).await;
    let curve = corrected_grab(&db, &app, &token, from).await;
    let stream = stream_of(&db).await;

    let (status, body) = pin(&app, &token, stream, from, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(curve_of(&db).await, Some(curve));
}
