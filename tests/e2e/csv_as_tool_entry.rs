//! S4, CSV import as data entry (PLAN.md story catalog).
//!
//! Scenario: a member imports a result sheet. Already-processed values are stored as served: no
//! calibration is stamped onto them and nothing recomputes them, because a stored calibration id
//! claims the row's `raw_value` is uncorrected input (ADR 0003) — importing a computed value used
//! to stamp the deployed sensor's calibration and recompute over it, and S4b pins that bug fixed.
//! Declared-raw imports keep the old behaviour: the covering calibration is stamped and applied.
//!
//! S4a (a CSV of raw tool inputs running the tool itself, with a tool_runs row and the same
//! provenance blob a typed entry gets) lands with Phase 3 and is encoded ignored below.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

/// Poll until the import worker has landed `want` readings at SITE1.
async fn poll_readings(db: &DatabaseConnection, time: &str, want: i64) -> i64 {
    let sql = format!(
        "SELECT count(*) AS n FROM readings WHERE site_id = '{}' AND time = '{time}'",
        crate::common::SITE1_ID
    );
    for _ in 0..100 {
        let row = db
            .query_one(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                sql.clone(),
            ))
            .await
            .unwrap()
            .unwrap();
        let n: i64 = row.try_get("", "n").unwrap();
        if n >= want {
            return n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    -1
}

fn deployed_probe() -> (&'static str, f64, f64) {
    ("S4-probe-01", 2.0, 1.0)
}

async fn deploy_calibrated_probe(db: &DatabaseConnection) {
    let (name, slope, intercept) = deployed_probe();
    let sensor =
        crate::common::sensor_lifecycle::create_sensor(db, name, crate::common::GLOBAL_PARAM_DO_ID)
            .await;
    crate::common::sensor_lifecycle::add_calibration(
        db,
        sensor.id,
        slope,
        intercept,
        crate::common::sensor_lifecycle::dt("2025-01-01T00:00:00Z"),
    )
    .await;
    crate::common::sensor_lifecycle::deploy_sensor(
        db,
        sensor.id,
        crate::common::SITE1_ID,
        crate::common::sensor_lifecycle::dt("2025-01-01T00:00:00Z"),
    )
    .await;
}

async fn imported_row(db: &DatabaseConnection, time: &str) -> (f64, Option<f64>, Option<uuid::Uuid>) {
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT raw_value, calibrated_value, calibration_id FROM readings \
                 WHERE site_id = '{}' AND parameter_id = '{}' AND time = '{time}'",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_DO_ID
            ),
        ))
        .await
        .unwrap()
        .expect("the imported reading exists");
    (
        row.try_get("", "raw_value").unwrap(),
        row.try_get("", "calibrated_value").unwrap(),
        row.try_get("", "calibration_id").unwrap(),
    )
}

/// S4b. Expected behaviour: an import of processed values under a covering deployment stores them
/// untouched — no calibration id, no recomputation — although the very same rows imported as
/// declared-raw get the calibration stamped and applied.
#[tokio::test]
#[serial]
async fn a_processed_import_is_not_recalibrated_and_a_raw_one_is() {
    let (db, app, token) = setup().await;
    deploy_calibrated_probe(&db).await;

    // Default: processed values. The deployment covers the instant, and still no correction.
    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": "DateTime,Dissolved_O2\n2025-06-01 00:00:00,250\n",
            "measurement_type": "spot",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "processed import ({status}): {resp}");
    assert_eq!(poll_readings(&db, "2025-06-01T00:00:00Z", 1).await, 1);

    let (raw, calibrated, calibration_id) = imported_row(&db, "2025-06-01T00:00:00Z").await;
    assert_eq!(raw, 250.0);
    assert_eq!(
        calibrated, None,
        "a processed value is not corrected again; the import may not claim a calibration"
    );
    assert_eq!(calibration_id, None);

    // Declared raw: the same shape opts into the covering calibration.
    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": "DateTime,Dissolved_O2\n2025-06-02 00:00:00,250\n",
            "measurement_type": "spot",
            "values": "raw",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "raw import ({status}): {resp}");
    assert_eq!(poll_readings(&db, "2025-06-02T00:00:00Z", 1).await, 1);

    let (raw, calibrated, calibration_id) = imported_row(&db, "2025-06-02T00:00:00Z").await;
    assert_eq!(raw, 250.0);
    assert_eq!(calibrated, Some(501.0), "2 * 250 + 1");
    assert!(calibration_id.is_some(), "the applied calibration is recorded");
}

/// Expected behaviour: a request field the server does not know is refused by name, never
/// silently dropped — the guard that keeps a version skew from eating a declaration.
#[tokio::test]
#[serial]
async fn an_unknown_import_field_is_refused_not_dropped() {
    let (_db, app, token) = setup().await;

    let (status, resp) = crate::common::post_json_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": "DateTime,Dissolved_O2\n2025-06-01 00:00:00,250\n",
            "value_state": "raw",
        }),
        &token,
    )
    .await;
    assert_eq!(
        status, 422,
        "a misspelled declaration is refused naming the field: {resp}"
    );
    assert!(resp.contains("value_state"), "{resp}");
}

/// S4a: importing raw tool inputs runs the tool over each row — same write path, same `tool_runs`
/// row and server-built provenance blob as a typed entry, source recorded as the import.
#[tokio::test]
#[serial]
#[ignore = "BLOCKED: CSV-as-tool-entry (run the tool over imported inputs) lands with Phase 3"]
async fn an_import_of_raw_inputs_runs_the_tool_and_carries_its_provenance() {
    unimplemented!(
        "import a CSV of DOC replicates declared as inputs of the doc tool -> one tool_runs row \
         per data row, readings via the grab write path, samples rows carrying the server-built \
         blob with source csv_import"
    );
}
