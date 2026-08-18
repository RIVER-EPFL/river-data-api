//! `POST /grab_samples` with `dry_run` returns the computed result without writing.
//!
//! Scenario: the entry form previews a grab while the operator types.
//! Expected behaviour: the response carries the raw value, each curve with its equation, the
//! composed line and the calibrated value, computed by the same code the save uses, and the
//! database is untouched until the save itself.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::sensor_lifecycle::{add_calibration, create_sensor, dt, seed_base_entities};
use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const GRAB_TIME: &str = "2025-06-15T10:00:00Z";

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

fn assert_close(actual: f64, expected: f64, what: &str) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "{what}: expected {expected}, got {actual}"
    );
}

#[tokio::test]
#[serial]
async fn dry_run_returns_the_result_and_writes_nothing() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor = create_sensor(&db, "Preview-plate-01", GLOBAL_PARAM_TEMP_ID).await;
    add_calibration(&db, sensor.id, 3.0, 2.0, dt("2025-01-01T00:00:00Z")).await;
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/standard_curves",
        &json!({ "sensor_id": sensor.id, "name": "Plate P", "slope": 2.0, "intercept": 0.5 }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "curve create: {body}");
    let curve: serde_json::Value = serde_json::from_str(&body).unwrap();
    let curve_id = curve["id"].as_str().unwrap();

    let payload = json!({
        "site_id": SITE1_ID,
        "dry_run": true,
        "readings": [{
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "sensor_id": sensor.id,
            "standard_curve_id": curve_id,
            "value": 10.0,
            "time": GRAB_TIME,
        }],
    });
    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &payload, &token).await;
    assert_eq!(status, 200, "dry run ({status}): {body}");
    let resp: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(resp["dry_run"], true);
    assert_eq!(resp["inserted"], 0);

    let p = &resp["preview"][0];
    assert_close(p["raw_value"].as_f64().unwrap(), 10.0, "raw value");
    assert_close(
        p["base_calibration"]["slope"].as_f64().unwrap(),
        3.0,
        "base slope",
    );
    assert_eq!(p["base_calibration"]["equation"], "y = 3x + 2");
    assert_eq!(p["standard_curve"]["name"], "Plate P");
    assert_eq!(p["standard_curve"]["equation"], "y = 2x + 0.5");
    assert_eq!(p["composed_equation"], "y = 6x + 4.5");
    assert_close(
        p["calibrated_value"].as_f64().unwrap(),
        64.5,
        "base then curve: 2.0 * (3.0 * 10.0 + 2.0) + 0.5",
    );

    assert_eq!(
        count(
            &db,
            &format!("SELECT COUNT(*) AS n FROM readings WHERE site_id = '{SITE1_ID}'")
        )
        .await,
        0,
        "a dry run stores no readings"
    );
    assert_eq!(
        count(
            &db,
            &format!("SELECT COUNT(*) AS n FROM samples WHERE site_id = '{SITE1_ID}'")
        )
        .await,
        0,
        "and no sample rows"
    );

    let mut save = payload;
    save["dry_run"] = json!(false);
    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &save, &token).await;
    assert_eq!(status, 200, "save ({status}): {body}");
    let saved: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(saved["inserted"], 1);
    assert_close(
        saved["preview"][0]["calibrated_value"].as_f64().unwrap(),
        64.5,
        "the save reports the numbers the dry run previewed",
    );

    let stored = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT calibrated_value FROM readings \
                 WHERE site_id = '{SITE1_ID}' AND time = '{GRAB_TIME}'"
            ),
        ))
        .await
        .unwrap()
        .expect("the save stored the reading");
    assert_close(
        stored
            .try_get::<Option<f64>>("", "calibrated_value")
            .unwrap()
            .expect("curves applied"),
        64.5,
        "the stored value equals the previewed value",
    );
}

#[tokio::test]
#[serial]
async fn a_negative_intercept_reads_as_a_subtraction() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor = create_sensor(&db, "Preview-plate-02", GLOBAL_PARAM_TEMP_ID).await;
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/standard_curves",
        &json!({ "sensor_id": sensor.id, "name": "Plate N", "slope": 0.9, "intercept": -0.1 }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "curve create: {body}");
    let curve: serde_json::Value = serde_json::from_str(&body).unwrap();

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "dry_run": true,
            "readings": [{
                "parameter_id": GLOBAL_PARAM_TEMP_ID,
                "sensor_id": sensor.id,
                "standard_curve_id": curve["id"],
                "value": 10.0,
                "time": GRAB_TIME,
            }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "dry run ({status}): {body}");
    let resp: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        resp["preview"][0]["standard_curve"]["equation"], "y = 0.9x - 0.1",
        "no 'x + -0.1' rendering: {resp}"
    );
}
