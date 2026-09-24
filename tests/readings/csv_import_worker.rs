//! CSV import runs on the worker pool: the handler stages the parsed rows and enqueues a
//! `csv_import` job; a worker claims it, inserts the readings, recomputes derived values, and the
//! staging rows are deleted, so a dead replica strands nothing in memory.
//!
//! Run with: cargo test --test readings

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

const CSV: &str = "DateTime,Dissolved_O2,DO_Temperature\n\
2025-06-01 00:00:00,250,12.0\n\
2025-06-01 00:10:00,300,12.5\n";

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

async fn poll_count(db: &DatabaseConnection, sql: &str, want: i64, max_secs: u64) -> i64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(max_secs);
    loop {
        let n = scalar_i64(db, sql).await;
        if n == want || std::time::Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

#[tokio::test]
#[serial]
async fn csv_import_runs_on_worker_and_clears_staging() {
    let (db, app, token) = setup().await;

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": crate::common::SITE1_ID, "csv": CSV }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");
    let job_id = resp["derived_job_id"]
        .as_str()
        .expect("a worker job id is returned");

    // The handler stages the rows; the worker reads them back. (Two parameters x two rows = 4 rows.)
    // It races with the background worker, so just assert the readings land and staging is drained.
    let readings = poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings WHERE site_id = '{}' AND time >= '2025-06-01T00:00:00Z'",
            crate::common::SITE1_ID
        ),
        4,
        10,
    )
    .await;
    assert_eq!(
        readings, 4,
        "all four staged readings should be inserted by the worker"
    );

    let staging_left = poll_count(&db, "SELECT count(*) AS n FROM csv_import_staging", 0, 10).await;
    assert_eq!(
        staging_left, 0,
        "the worker deletes its staged rows on completion"
    );

    // Staging is dropped inside the run, so the row can still read `running` for a moment after.
    let completed = poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM reprocessing_jobs \
             WHERE id = '{job_id}' AND status = 'completed'"
        ),
        1,
        10,
    )
    .await;
    assert_eq!(completed, 1, "the csv_import job reaches completed");
}

const CSV_DUP_TS: &str = "DateTime,Dissolved_O2,DO_Temperature\n\
2025-06-01 00:00:00,250,12.0\n\
2025-06-01 00:00:00,260,12.5\n";

async fn scalar_f64(db: &DatabaseConnection, sql: &str) -> f64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<f64>("", "v")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn csv_import_duplicate_timestamps_become_replicates() {
    let (db, app, token) = setup().await;

    // Rows sharing a timestamp for the same parameter are replicates 0..n-1 in file order and, the
    // file being declared spot, are grouped into a sample. The distinct replicate indices also keep
    // the conflict keys unique, so overwrite mode's `ON CONFLICT DO UPDATE` cannot fail with
    // "cannot affect row a second time".
    let (status, resp) = crate::common::post_screened_import(
        &app,
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": CSV_DUP_TS,
            "conflict": "overwrite",
            "measurement_type": "spot",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");
    let job_id = resp["derived_job_id"]
        .as_str()
        .expect("a worker job id is returned");

    let staging_left = poll_count(&db, "SELECT count(*) AS n FROM csv_import_staging", 0, 10).await;
    assert_eq!(
        staging_left, 0,
        "staging is drained even with a duplicated timestamp"
    );

    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT status FROM reprocessing_jobs WHERE id = '{job_id}'"),
        ))
        .await
        .unwrap()
        .unwrap();
    let job_status: String = row.try_get("", "status").unwrap();
    assert_eq!(
        job_status, "completed",
        "the duplicate-timestamp import completes, not fails"
    );

    let at_ts = scalar_i64(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE site_id = '{}' AND time = '2025-06-01T00:00:00Z'",
            crate::common::SITE1_ID
        ),
    )
    .await;
    assert_eq!(at_ts, 4, "two replicates per parameter, no rows collapsed");

    let do_value = scalar_f64(
        &db,
        &format!(
            "SELECT r.raw_value AS v FROM readings r \
             JOIN parameters p ON p.id = r.parameter_id \
             WHERE r.site_id = '{}' AND p.code = 'Dissolved_O2' \
               AND r.time = '2025-06-01T00:00:00Z' AND r.replicate_index = 1",
            crate::common::SITE1_ID
        ),
    )
    .await;
    assert!(
        (do_value - 260.0).abs() < 1e-9,
        "replicates numbered in file order: got {do_value}"
    );

    let sample_mean = scalar_f64(
        &db,
        &format!(
            "SELECT s.mean AS v FROM samples s \
             JOIN parameters p ON p.id = s.parameter_id \
             WHERE s.site_id = '{}' AND p.code = 'Dissolved_O2' \
               AND s.collected_at = '2025-06-01T00:00:00Z'",
            crate::common::SITE1_ID
        ),
    )
    .await;
    assert!(
        (sample_mean - 255.0).abs() < 1e-9,
        "replicate group formed a sample: got {sample_mean}"
    );
}

#[tokio::test]
#[serial]
async fn csv_import_recomputes_derived_via_worker() {
    let (db, app, token) = setup().await;

    // A derived parameter (DO mg/L = Dissolved_O2 * 0.032) assigned to the site, so the imported
    // Dissolved_O2 rows produce derived readings when the worker recomputes them.
    let derived_name = format!("DOmgL_{}", Uuid::new_v4().simple());
    let calculation =
        crate::common::seed_formula_calculation(&db, &format!("{derived_name}_set")).await;
    let (_s, def) = crate::common::post_json_parse_with_token(
        &app,
        "/api/derived_parameters",
        &serde_json::json!({
            "code": derived_name, "name": "DO mg/L", "units": "mg/L",
            "formula": "Dissolved_O2 * 0.032", "tool_script_id": calculation,
        }),
        &token,
    )
    .await;
    let output_parameter_id = def["output_parameter_id"].as_str().unwrap().to_string();
    crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID, "parameter_id": output_parameter_id, "name": derived_name,
            "sensor_type": "derived", "entry_mode": "tool",
        }),
        &token,
    )
    .await;

    let (status, _resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": crate::common::SITE1_ID, "csv": CSV }),
        &token,
    )
    .await;
    assert_eq!(status, 200);

    // The worker's Phase 2 recompute writes the derived parameter's readings.
    let derived = poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE parameter_id = '{output_parameter_id}' AND time >= '2025-06-01T00:00:00Z'"
        ),
        2,
        10,
    )
    .await;
    assert_eq!(
        derived, 2,
        "the worker recomputes a derived value per imported timestamp"
    );
}

const CSV_TRIPLICATE: &str = "DateTime,Dissolved_O2\n\
2025-06-02 00:00:00,100\n\
2025-06-02 00:00:00,110\n\
2025-06-02 00:00:00,120\n";

#[tokio::test]
#[serial]
async fn csv_import_triplicate_rows_form_sample_and_reimport_is_idempotent() {
    let (db, app, token) = setup().await;

    let (status, resp) = crate::common::post_screened_import(
        &app,
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": CSV_TRIPLICATE,
            "measurement_type": "spot",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");
    assert_eq!(
        resp["inserted_total"], 3,
        "intra-group rows are not reported as duplicates: {resp}"
    );
    assert_eq!(resp["duplicates"], 0, "{resp}");

    let readings = poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE site_id = '{}' AND time = '2025-06-02T00:00:00Z'",
            crate::common::SITE1_ID
        ),
        3,
        10,
    )
    .await;
    assert_eq!(readings, 3, "three replicate readings inserted");

    let samples = scalar_i64(
        &db,
        &format!(
            "SELECT count(*) AS n FROM samples \
             WHERE site_id = '{}' AND collected_at = '2025-06-02T00:00:00Z'",
            crate::common::SITE1_ID
        ),
    )
    .await;
    assert_eq!(samples, 1, "one sample per replicate group");

    let stamped = scalar_i64(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE site_id = '{}' AND time = '2025-06-02T00:00:00Z' AND sample_id IS NOT NULL",
            crate::common::SITE1_ID
        ),
    )
    .await;
    assert_eq!(stamped, 3, "every replicate references the sample");

    let mean = scalar_f64(
        &db,
        &format!(
            "SELECT mean AS v FROM samples \
             WHERE site_id = '{}' AND collected_at = '2025-06-02T00:00:00Z'",
            crate::common::SITE1_ID
        ),
    )
    .await;
    assert!(
        (mean - 110.0).abs() < 1e-9,
        "trigger populated the sample mean: got {mean}"
    );

    let (status, resp) = crate::common::post_screened_import(
        &app,
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": CSV_TRIPLICATE,
            "measurement_type": "spot",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "re-import ({status}): {resp}");
    assert_eq!(
        resp["inserted_total"], 0,
        "re-import inserts nothing: {resp}"
    );
    assert_eq!(
        resp["duplicates"], 3,
        "the whole file overlaps identically: {resp}"
    );

    let readings_after = scalar_i64(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE site_id = '{}' AND time = '2025-06-02T00:00:00Z'",
            crate::common::SITE1_ID
        ),
    )
    .await;
    assert_eq!(readings_after, 3, "no duplicate rows on re-import");

    let samples_after = scalar_i64(
        &db,
        &format!(
            "SELECT count(*) AS n FROM samples \
             WHERE site_id = '{}' AND collected_at = '2025-06-02T00:00:00Z'",
            crate::common::SITE1_ID
        ),
    )
    .await;
    assert_eq!(samples_after, 1, "the sample is reused, not duplicated");
}

const CSV_SINGLE_GRABS: &str = "DateTime,Dissolved_O2\n\
2025-06-03 09:00:00,140\n\
2025-06-03 10:00:00,150\n";

/// Expected behaviour: a file declared `spot` is a set of collection events, and a row measured
/// once is a single measurement, so no `samples` row is minted around it. Everything reading grabs
/// derives n = 1 from the reading itself.
#[tokio::test]
#[serial]
async fn declared_spot_import_mints_no_sample_for_a_lone_row() {
    let (db, app, token) = setup().await;

    let (status, resp) = crate::common::post_screened_import(
        &app,
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": CSV_SINGLE_GRABS,
            "measurement_type": "spot",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");

    let readings = poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE site_id = '{}' AND time >= '2025-06-03T00:00:00Z' \
               AND measurement_type = 'spot'",
            crate::common::SITE1_ID
        ),
        2,
        10,
    )
    .await;
    assert_eq!(readings, 2, "both grabs land as spot readings");

    let samples = scalar_i64(
        &db,
        &format!(
            "SELECT count(*) AS n FROM samples \
             WHERE site_id = '{}' AND collected_at >= '2025-06-03T00:00:00Z'",
            crate::common::SITE1_ID
        ),
    )
    .await;
    assert_eq!(samples, 0, "a row measured once forms no sample");

    let value = scalar_f64(
        &db,
        &format!(
            "SELECT COALESCE(calibrated_value, raw_value) AS v FROM readings \
             WHERE site_id = '{}' AND time = '2025-06-03T09:00:00Z'",
            crate::common::SITE1_ID
        ),
    )
    .await;
    assert!(
        (value - 140.0).abs() < 1e-9,
        "the measurement is served from the reading: got {value}"
    );
}

const CSV_CONTINUOUS_DUP: &str = "DateTime,Dissolved_O2\n\
2025-06-04 00:00:00,200\n\
2025-06-04 00:00:00,210\n";

/// Expected behaviour: two logger points sharing a timestamp are a malformed file, not a sampling
/// event. The cadence the write resolves decides that, not what the request declared, so a file
/// declaring nothing is refused the repeat too and no `samples` row is invented around it.
#[tokio::test]
#[serial]
async fn continuous_rows_sharing_a_timestamp_are_refused() {
    let (db, app, token) = setup().await;

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": crate::common::SITE1_ID, "csv": CSV_CONTINUOUS_DUP }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");

    let errors = resp["errors"].as_array().expect("errors array");
    assert!(
        errors.iter().any(|e| {
            e["row"].as_u64() == Some(3)
                && e["message"].as_str().unwrap_or("").contains("is repeated")
        }),
        "the repeated timestamp is reported against its line: {resp}"
    );

    let readings = poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE site_id = '{}' AND time = '2025-06-04T00:00:00Z'",
            crate::common::SITE1_ID
        ),
        1,
        10,
    )
    .await;
    assert_eq!(readings, 1, "only the first row of the repeat is stored");

    let staging_left = poll_count(&db, "SELECT count(*) AS n FROM csv_import_staging", 0, 10).await;
    assert_eq!(staging_left, 0, "the import ran to completion");

    let samples = scalar_i64(
        &db,
        &format!(
            "SELECT count(*) AS n FROM samples \
             WHERE site_id = '{}' AND collected_at = '2025-06-04T00:00:00Z'",
            crate::common::SITE1_ID
        ),
    )
    .await;
    assert_eq!(samples, 0, "no sample around undeclared logger duplicates");
}

const CSV_GRAB: &str = "DateTime,Dissolved_O2\n\
2025-07-01 09:00:00,250\n";

async fn grab_row(db: &DatabaseConnection) -> (Option<f64>, Option<Uuid>, Option<Uuid>) {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT calibrated_value, calibration_id, standard_curve_id FROM readings \
                 WHERE site_id = '{}' AND time = '2025-07-01T09:00:00Z'",
                crate::common::SITE1_ID
            ),
        ))
        .await
        .unwrap()
        .expect("the imported grab is stored");
    (
        row.try_get("", "calibrated_value").unwrap(),
        row.try_get("", "calibration_id").unwrap(),
        row.try_get("", "standard_curve_id").unwrap(),
    )
}

/// Declared raw: these tests are about what the slot's calibration does to uncorrected input.
async fn import_grab_csv(app: &axum::Router, token: &str) {
    let (status, resp) = crate::common::post_screened_import(
        app,
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": CSV_GRAB,
            "measurement_type": "spot",
            "values": "raw",
        }),
        token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");
}

/// A stored `calibrated_value` says a curve produced it. An imported grab whose slot resolves no
/// calibration has none, so the column stays null: repeating the raw value there would claim a
/// correction was applied, and nothing downstream could ever repair it, since a grab is outside
/// window resolution by construction.
#[tokio::test]
#[serial]
async fn an_imported_grab_with_no_curve_stores_no_corrected_value() {
    let (db, app, token) = setup().await;

    import_grab_csv(&app, &token).await;
    let inserted = poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE site_id = '{}' AND time = '2025-07-01T09:00:00Z'",
            crate::common::SITE1_ID
        ),
        1,
        10,
    )
    .await;
    assert_eq!(inserted, 1, "the worker inserts the staged grab");

    let (calibrated, calibration_id, standard_curve_id) = grab_row(&db).await;
    assert_eq!(
        calibrated, None,
        "no curve resolved, so no corrected value is invented"
    );
    assert_eq!(calibration_id, None);
    assert_eq!(standard_curve_id, None);
}

/// The paired case: the slot's calibration is applied and recorded, so the value and the reference
/// beside it agree.
#[tokio::test]
#[serial]
async fn an_imported_grab_uses_the_slot_calibration_it_records() {
    let (db, app, token) = setup().await;

    let sensor = crate::common::sensor_lifecycle::create_sensor(
        &db,
        "Lab-probe-01",
        crate::common::GLOBAL_PARAM_DO_ID,
    )
    .await;
    let calibration = crate::common::sensor_lifecycle::add_calibration(
        &db,
        sensor.id,
        2.0,
        1.0,
        crate::common::sensor_lifecycle::dt("2025-01-01T00:00:00Z"),
    )
    .await;
    crate::common::sensor_lifecycle::deploy_sensor(
        &db,
        sensor.id,
        crate::common::SITE1_ID,
        crate::common::sensor_lifecycle::dt("2025-01-01T00:00:00Z"),
    )
    .await;

    import_grab_csv(&app, &token).await;
    poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE site_id = '{}' AND time = '2025-07-01T09:00:00Z'",
            crate::common::SITE1_ID
        ),
        1,
        10,
    )
    .await;

    let (calibrated, calibration_id, standard_curve_id) = grab_row(&db).await;
    assert_eq!(
        calibration_id,
        Some(calibration),
        "the grab records the calibration its slot resolved"
    );
    assert_eq!(
        calibrated,
        Some(501.0),
        "and serves what that calibration produces from the measured value"
    );
    assert_eq!(
        standard_curve_id, None,
        "an import picks no lab curve; that is an operator's choice per measurement"
    );
}

// ============================================================================
// Overwrite corrects the measurement, not the correction
// ============================================================================

const CSV_DO_AT: &str = "DateTime,Dissolved_O2\n2025-06-01 00:00:00,250\n";
const CSV_DO_CORRECTED: &str = "DateTime,Dissolved_O2\n2025-06-01 00:00:00,400\n";

/// `(raw_value, calibrated_value, calibration_id, standard_curve_id)` at one slot.
async fn row_at(
    db: &DatabaseConnection,
    parameter_id: &str,
    time: &str,
) -> (f64, Option<f64>, Option<Uuid>, Option<Uuid>) {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT raw_value, calibrated_value, calibration_id, standard_curve_id \
                 FROM readings WHERE site_id = '{}' AND parameter_id = '{parameter_id}' \
                 AND time = '{time}'",
                crate::common::SITE1_ID
            ),
        ))
        .await
        .unwrap()
        .expect("the slot holds a reading");
    (
        row.try_get("", "raw_value").unwrap(),
        row.try_get("", "calibrated_value").unwrap(),
        row.try_get("", "calibration_id").unwrap(),
        row.try_get("", "standard_curve_id").unwrap(),
    )
}

async fn import_csv(
    app: &axum::Router,
    token: &str,
    body: &serde_json::Value,
) -> serde_json::Value {
    let (status, resp) = crate::common::post_screened_import(app, body, token).await;
    assert_eq!(status, 200, "import ({status}): {resp}");
    resp
}

/// Expected behaviour: the corrected measurement is put back through the calibration the row
/// already carries, even once no deployment covers that instant any more. An import never re-decides
/// which curve applies, so the reference and the value beside it stay in agreement.
#[tokio::test]
#[serial]
async fn an_overwrite_recomputes_from_the_calibration_the_row_carries() {
    let (db, app, token) = setup().await;

    let sensor = crate::common::sensor_lifecycle::create_sensor(
        &db,
        "Overwrite-probe-01",
        crate::common::GLOBAL_PARAM_DO_ID,
    )
    .await;
    let calibration = crate::common::sensor_lifecycle::add_calibration(
        &db,
        sensor.id,
        2.0,
        1.0,
        crate::common::sensor_lifecycle::dt("2025-01-01T00:00:00Z"),
    )
    .await;
    let deployment = crate::common::sensor_lifecycle::deploy_sensor(
        &db,
        sensor.id,
        crate::common::SITE1_ID,
        crate::common::sensor_lifecycle::dt("2025-01-01T00:00:00Z"),
    )
    .await;

    import_csv(
        &app,
        &token,
        &serde_json::json!({ "site": crate::common::SITE1_ID, "csv": CSV_DO_AT, "values": "raw" }),
    )
    .await;
    poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings WHERE site_id = '{}' \
             AND parameter_id = '{}' AND time = '2025-06-01T00:00:00Z'",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_DO_ID
        ),
        1,
        10,
    )
    .await;

    let (raw, calibrated, stored_calibration, _) = row_at(
        &db,
        crate::common::GLOBAL_PARAM_DO_ID,
        "2025-06-01T00:00:00Z",
    )
    .await;
    assert_eq!(raw, 250.0);
    assert_eq!(calibrated, Some(501.0), "2 * 250 + 1");
    assert_eq!(stored_calibration, Some(calibration));

    // The instrument is recalled before the correction arrives, so window resolution no longer
    // reaches this instant.
    crate::common::sensor_lifecycle::end_deployment(
        &db,
        deployment,
        crate::common::sensor_lifecycle::dt("2025-02-01T00:00:00Z"),
    )
    .await;

    import_csv(
        &app,
        &token,
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": CSV_DO_CORRECTED,
            "conflict": "overwrite",
        }),
    )
    .await;
    poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings WHERE site_id = '{}' \
             AND parameter_id = '{}' AND time = '2025-06-01T00:00:00Z' \
             AND raw_value = 400 AND calibrated_value = 801",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_DO_ID
        ),
        1,
        10,
    )
    .await;

    let (raw, calibrated, stored_calibration, _) = row_at(
        &db,
        crate::common::GLOBAL_PARAM_DO_ID,
        "2025-06-01T00:00:00Z",
    )
    .await;
    assert_eq!(raw, 400.0, "the correction replaced the measurement");
    assert_eq!(
        stored_calibration,
        Some(calibration),
        "and left the calibration the row was corrected by"
    );
    assert_eq!(
        calibrated,
        Some(801.0),
        "2 * 400 + 1: the corrected value comes from that same calibration"
    );
}

/// Expected behaviour: a lab curve is chosen by hand and no window query can recover it, so
/// correcting the measurement it explains keeps it and recomputes through it.
#[tokio::test]
#[serial]
async fn an_overwritten_grab_keeps_the_lab_curve_it_was_measured_against() {
    let (db, app, token) = setup().await;

    let sensor_id = "00000000-0000-4000-c000-0000000000a1";
    let curve_id = "00000000-0000-4000-c000-0000000000b1";
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, is_active, is_lab_instrument, created_at) \
             VALUES ('{sensor_id}', 'Microplate reader', true, true, now())"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept, name) \
             VALUES ('{curve_id}', '{sensor_id}', 3.0, 0.5, 'Plate A')"
        ),
    )
    .await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "readings": [{
                "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
                "sensor_id": sensor_id,
                "standard_curve_id": curve_id,
                "value": 10.0,
                "time": "2025-07-01T09:00:00Z",
            }]
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab with a lab curve: {body}");

    let (raw, calibrated, _, stored_curve) = row_at(
        &db,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "2025-07-01T09:00:00Z",
    )
    .await;
    assert_eq!(raw, 10.0);
    assert_eq!(calibrated, Some(30.5), "3 * 10 + 0.5");
    assert_eq!(stored_curve, Some(curve_id.parse::<Uuid>().unwrap()));

    import_csv(
        &app,
        &token,
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": "DateTime,DO_Temperature\n2025-07-01 09:00:00,20\n",
            "conflict": "overwrite",
            "measurement_type": "spot",
        }),
    )
    .await;
    poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings WHERE site_id = '{}' \
             AND parameter_id = '{}' AND time = '2025-07-01T09:00:00Z' \
             AND raw_value = 20 AND calibrated_value = 60.5",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
        1,
        10,
    )
    .await;

    let (raw, calibrated, _, stored_curve) = row_at(
        &db,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "2025-07-01T09:00:00Z",
    )
    .await;
    assert_eq!(raw, 20.0, "the correction replaced the measurement");
    assert_eq!(
        stored_curve,
        Some(curve_id.parse::<Uuid>().unwrap()),
        "the operator's curve survives an import that never picked one"
    );
    assert_eq!(
        calibrated,
        Some(60.5),
        "3 * 20 + 0.5: recomputed through that curve"
    );
}

const CSV_TRIPLICATE_CORRECTED: &str = "DateTime,Dissolved_O2\n\
2025-06-02 00:00:00,200\n\
2025-06-02 00:00:00,300\n";

/// An overwrite of a spot file replaces each replicate set whole: a stored replicate beyond the
/// incoming count would otherwise survive positionally and keep double-counting the group.
#[tokio::test]
#[serial]
async fn csv_overwrite_replaces_the_whole_replicate_set() {
    let (db, app, token) = setup().await;

    let (status, resp) = crate::common::post_screened_import(
        &app,
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": CSV_TRIPLICATE,
            "measurement_type": "spot",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");
    poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE site_id = '{}' AND time = '2025-06-02T00:00:00Z'",
            crate::common::SITE1_ID
        ),
        3,
        10,
    )
    .await;

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": CSV_TRIPLICATE_CORRECTED,
            "measurement_type": "spot",
            "dry_run": true,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "plan ({status}): {plan}");
    assert_eq!(
        plan["replicate_groups"], 1,
        "the plan reports the detected replicate group: {plan}"
    );

    let (status, resp) = crate::common::post_screened_import(
        &app,
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": CSV_TRIPLICATE_CORRECTED,
            "measurement_type": "spot",
            "conflict": "overwrite",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "overwrite import ({status}): {resp}");

    let served = poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE site_id = '{}' AND time = '2025-06-02T00:00:00Z' AND withdrawn_at IS NULL",
            crate::common::SITE1_ID
        ),
        2,
        10,
    )
    .await;
    assert_eq!(
        served, 2,
        "the third stored replicate leaves the group the two-row correction describes"
    );

    let stored = scalar_i64(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE site_id = '{}' AND time = '2025-06-02T00:00:00Z'",
            crate::common::SITE1_ID
        ),
    )
    .await;
    assert_eq!(
        stored, 3,
        "it leaves by a reversible stamp, not by a delete"
    );

    let mean = scalar_f64(
        &db,
        &format!(
            "SELECT mean AS v FROM samples \
             WHERE site_id = '{}' AND collected_at = '2025-06-02T00:00:00Z'",
            crate::common::SITE1_ID
        ),
    )
    .await;
    assert!(
        (mean - 250.0).abs() < 1e-9,
        "(200 + 300) / 2 over the replacement set: got {mean}"
    );
}

/// Scenario: the import's first run fails on a transient error inserting its readings.
/// Expected behaviour: the staged rows wait for the retry, which lands every reading; the job does
/// not read as completed with nothing imported.
#[tokio::test]
#[serial]
async fn a_csv_import_that_fails_once_lands_its_rows_on_the_retry() {
    use river_db::routes::private::reprocessing_jobs::service as jobs;
    let (db, app, token) = setup().await;
    // This test is the worker, under a policy with a retry to spend and no backoff to wait out.
    crate::common::stop_test_workers().await;

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": crate::common::SITE1_ID, "csv": CSV }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");
    let job_id = resp["derived_job_id"]
        .as_str()
        .expect("a worker job id is returned")
        .to_string();

    for sql in [
        "CREATE SEQUENCE test_csv_fail_once",
        "CREATE OR REPLACE FUNCTION test_csv_fail_once() RETURNS trigger AS \
         $$ BEGIN IF nextval('test_csv_fail_once') = 1 THEN RAISE EXCEPTION 'transient'; END IF; \
         RETURN NEW; END; $$ LANGUAGE plpgsql",
        "CREATE TRIGGER test_csv_fail_once BEFORE INSERT ON readings \
         FOR EACH ROW EXECUTE FUNCTION test_csv_fail_once()",
    ] {
        crate::common::exec(&db, sql).await;
    }

    let registry = jobs::build_registry();
    let events = tokio::sync::broadcast::channel::<river_db::common::AppEvent>(16).0;
    let worker = jobs::worker_id();
    let policy = jobs::RetryPolicy {
        max_retries: 1,
        backoff_base: std::time::Duration::ZERO,
    };
    while jobs::run_one_with_policy(&db, &events, &registry, &worker, policy)
        .await
        .unwrap()
    {}

    for sql in [
        "DROP TRIGGER test_csv_fail_once ON readings",
        "DROP FUNCTION test_csv_fail_once()",
        "DROP SEQUENCE test_csv_fail_once",
    ] {
        crate::common::exec(&db, sql).await;
    }

    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT count(*) AS n FROM reprocessing_jobs \
                 WHERE id = '{job_id}' AND status = 'completed' AND retry_count = 1"
            )
        )
        .await,
        1,
        "the import completes on its second attempt"
    );
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT count(*) AS n FROM readings WHERE site_id = '{}' \
                 AND time >= '2025-06-01T00:00:00Z'",
                crate::common::SITE1_ID
            )
        )
        .await,
        4,
        "the retry lands every staged reading"
    );
    assert_eq!(
        scalar_i64(&db, "SELECT count(*) AS n FROM csv_import_staging").await,
        0,
        "and then drops the staged rows"
    );
}

/// Rows of the monthly rollup at SITE1 for June 2025, as materialized rather than read through.
async fn materialized_june(db: &DatabaseConnection) -> i64 {
    scalar_i64(
        db,
        &format!(
            "SELECT count(*) AS n FROM readings_monthly WHERE site_id = '{}' \
             AND bucket = '2025-06-01T00:00:00Z'",
            crate::common::SITE1_ID
        ),
    )
    .await
}

/// Scenario: an import's first run stores its readings and then fails refreshing the rollups.
/// Expected behaviour: the retry, which finds the readings already stored and moves none of them,
/// still refreshes the rollups over what the failed attempt landed before it completes.
#[tokio::test]
#[serial]
async fn a_csv_import_whose_refresh_fails_refreshes_on_the_retry() {
    use river_db::routes::private::reprocessing_jobs::service as jobs;
    let (db, app, token) = setup().await;
    crate::common::stop_test_workers().await;

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": crate::common::SITE1_ID, "csv": CSV }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");
    let job_id = resp["derived_job_id"]
        .as_str()
        .expect("a worker job id is returned")
        .to_string();

    crate::common::exec(
        &db,
        "ALTER MATERIALIZED VIEW readings_monthly SET (timescaledb.materialized_only = true)",
    )
    .await;
    let registry = jobs::build_registry();
    let events = tokio::sync::broadcast::channel::<river_db::common::AppEvent>(16).0;
    let worker = jobs::worker_id();
    let policy = jobs::RetryPolicy {
        max_retries: 1,
        backoff_base: std::time::Duration::ZERO,
    };
    crate::common::jobs::refuse_refresh(&db).await;
    assert!(
        jobs::run_one_with_policy(&db, &events, &registry, &worker, policy)
            .await
            .unwrap(),
        "the import's first attempt runs"
    );
    crate::common::jobs::restore_refresh(&db).await;
    let queued = scalar_i64(
        &db,
        &format!(
            "SELECT count(*) AS n FROM reprocessing_jobs \
             WHERE id = '{job_id}' AND status = 'queued' AND retry_count = 1"
        ),
    )
    .await;
    let before = materialized_june(&db).await;

    while jobs::run_one_with_policy(&db, &events, &registry, &worker, policy)
        .await
        .unwrap()
    {}
    let after = materialized_june(&db).await;
    crate::common::exec(
        &db,
        "ALTER MATERIALIZED VIEW readings_monthly SET (timescaledb.materialized_only = false)",
    )
    .await;

    assert_eq!(queued, 1, "the failed refresh fails the first attempt");
    assert_eq!(
        before, 0,
        "nothing is materialized after the failed refresh"
    );
    assert!(
        after > 0,
        "the retry refreshes what the first attempt landed"
    );
}

/// Scenario: an import lands a temperature that a calculation at the site reads, and on its first
/// run the derived write recomputing it fails.
/// Expected behaviour: the job is not reported complete but queued again with its staged rows, and
/// the retry, which finds the readings already stored and moves none of them, still recomputes the
/// derived value.
#[tokio::test]
#[serial]
async fn a_csv_import_whose_derived_recompute_fails_recomputes_on_the_retry() {
    use river_db::routes::private::reprocessing_jobs::service as jobs;
    let (db, app, token) = setup().await;

    let calculation = crate::common::seed_formula_calculation(&db, "import_derived_set").await;
    let (status, def) = crate::common::post_json_parse_with_token(
        &app,
        "/api/derived_parameters",
        &serde_json::json!({
            "code": "TempDoubled_import", "name": "Temperature doubled", "units": "x",
            "formula": "DO_Temperature * 2", "tool_script_id": calculation,
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create derived ({status}): {def}"
    );
    let output = def["output_parameter_id"]
        .as_str()
        .expect("output_parameter_id")
        .to_string();
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID, "parameter_id": output,
            "name": "TempDoubled_import", "sensor_type": "derived", "entry_mode": "tool",
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "assign derived ({status}): {body}"
    );

    // This test is the worker, under a policy with a retry to spend and no backoff to wait out.
    crate::common::stop_test_workers().await;
    let registry = jobs::build_registry();
    let events = tokio::sync::broadcast::channel::<river_db::common::AppEvent>(16).0;
    let worker = jobs::worker_id();
    let policy = jobs::RetryPolicy {
        max_retries: 1,
        backoff_base: std::time::Duration::ZERO,
    };
    while jobs::run_one_with_policy(&db, &events, &registry, &worker, policy)
        .await
        .unwrap()
    {}

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": crate::common::SITE1_ID, "csv": CSV }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");
    let job_id = resp["derived_job_id"]
        .as_str()
        .expect("a worker job id is returned")
        .to_string();

    for sql in [
        "CREATE FUNCTION test_refuse_derived() RETURNS trigger AS $$ BEGIN \
         IF NEW.measurement_type = 'derived' THEN RAISE EXCEPTION 'derived write refused'; END IF; \
         RETURN NEW; END; $$ LANGUAGE plpgsql",
        "CREATE TRIGGER test_refuse_derived BEFORE INSERT OR UPDATE ON readings \
         FOR EACH ROW EXECUTE FUNCTION test_refuse_derived()",
    ] {
        crate::common::exec(&db, sql).await;
    }
    assert!(
        jobs::run_one_with_policy(&db, &events, &registry, &worker, policy)
            .await
            .unwrap(),
        "the import's first attempt runs"
    );
    for sql in [
        "DROP TRIGGER test_refuse_derived ON readings",
        "DROP FUNCTION test_refuse_derived()",
    ] {
        crate::common::exec(&db, sql).await;
    }
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT count(*) AS n FROM reprocessing_jobs \
                 WHERE id = '{job_id}' AND status = 'queued' AND retry_count = 1"
            )
        )
        .await,
        1,
        "an import whose recompute failed is queued again, not complete"
    );

    while jobs::run_one_with_policy(&db, &events, &registry, &worker, policy)
        .await
        .unwrap()
    {}
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT count(*) AS n FROM reprocessing_jobs \
                 WHERE id = '{job_id}' AND status = 'completed'"
            )
        )
        .await,
        1,
        "the retry completes"
    );
    // 2 * 12.0 and 2 * 12.5
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT count(*) AS n FROM readings WHERE parameter_id = '{output}' \
                 AND time >= '2025-06-01T00:00:00Z' AND raw_value IN (24, 25)"
            )
        )
        .await,
        2,
        "the retry recomputes both derived values from the stored temperatures"
    );
}

/// Scenario: one import's job failed without its body ever dropping what it staged, another is
/// still queued.
/// Expected behaviour: the sweep takes the failed import's rows and leaves the queued one's.
#[tokio::test]
#[serial]
async fn the_staging_sweep_takes_only_what_no_live_import_will_read() {
    let (db, _app, _token) = setup().await;
    crate::common::stop_test_workers().await;
    let (dead, live) = (Uuid::new_v4(), Uuid::new_v4());
    for (token, status) in [(dead, "failed"), (live, "queued")] {
        for sql in [
            format!(
                "INSERT INTO reprocessing_jobs (id, trigger_type, status, params) \
                 VALUES (gen_random_uuid(), 'csv_import', '{status}', \
                         '{{\"import_token\": \"{token}\"}}'::jsonb)"
            ),
            format!(
                "INSERT INTO csv_import_staging (import_token, seq, stream_id, time, raw_value) \
                 VALUES ('{token}', 0, '{}', '2025-06-01T00:00:00Z', 1.0)",
                crate::common::STREAM1_ID
            ),
        ] {
            crate::common::exec(&db, &sql).await;
        }
    }

    let swept = river_db::routes::private::readings::flows::prune_orphaned_staging(&db)
        .await
        .expect("the sweep runs");
    assert_eq!(swept, 1, "the failed import's row goes");
    assert_eq!(
        scalar_i64(
            &db,
            &format!("SELECT count(*) AS n FROM csv_import_staging WHERE import_token = '{live}'")
        )
        .await,
        1,
        "the queued import keeps what it will read"
    );
}

/// Run every queued job on this thread until none is left, the worker pool being stopped.
async fn run_queued_jobs(db: &DatabaseConnection) {
    use river_db::routes::private::reprocessing_jobs::service as jobs;
    let registry = jobs::build_registry();
    let events = tokio::sync::broadcast::channel::<river_db::common::AppEvent>(16).0;
    let worker = jobs::worker_id();
    while jobs::run_one_with_policy(
        db,
        &events,
        &registry,
        &worker,
        jobs::RetryPolicy::default(),
    )
    .await
    .unwrap()
    {}
}

/// The importer's channel for `param` at `site`, and the counts that say whether its rows agree
/// with its pairing.
struct Channel {
    stream_id: String,
    stored: String,
}

impl Channel {
    async fn of(db: &DatabaseConnection, site: &str, param: &str) -> Self {
        let row = db
            .query_one_raw(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT id::text AS id FROM data_streams \
                     WHERE source_system = 'api' AND source_key = '{site}:{param}'"
                ),
            ))
            .await
            .unwrap()
            .expect("the import opened the channel");
        let stream_id: String = row.try_get("", "id").unwrap();
        let stored =
            format!("SELECT count(*) AS n FROM readings r WHERE r.stream_id = '{stream_id}'");
        Self { stream_id, stored }
    }

    async fn stored(&self, db: &DatabaseConnection, filter: &str) -> i64 {
        scalar_i64(db, &format!("{} AND {filter}", self.stored)).await
    }
}

async fn import_column(app: &axum::Router, token: &str, code: &str) {
    let (status, resp) = crate::common::post_json_parse_with_token(
        app,
        "/api/readings/import_csv",
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": format!("DateTime,{code}\n2025-06-01 00:00:00,1.5\n2025-06-01 00:10:00,2.5\n"),
        }),
        token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");
    assert!(
        resp["derived_job_id"].is_string(),
        "the rows wait for the worker: {resp}"
    );
}

/// Scenario: a file is imported against a column whose channel is unpaired, and the channel is
/// paired before the import's job runs.
/// Expected behaviour: the rows land attributed to the slot the channel is paired to, and the slot
/// reprocess that resolves their deployment and curve is queued and runs.
#[tokio::test]
#[serial]
async fn a_channel_paired_before_the_import_runs_attributes_its_rows() {
    let (db, app, token) = setup().await;
    crate::common::stop_test_workers().await;
    let site = crate::common::SITE1_ID;
    let param =
        crate::common::e2e::create_parameter(&app, &token, "pairedlater", "Paired later", "m")
            .await;

    import_column(&app, &token, "pairedlater").await;
    let channel = Channel::of(&db, site, &param).await;
    let slot = crate::common::e2e::assign_site_parameter_minimal(&app, &token, site, &param).await;
    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{}/pair", channel.stream_id),
        &serde_json::json!({ "site_parameter_id": slot }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "pair it: {status} {text}");

    run_queued_jobs(&db).await;

    assert_eq!(channel.stored(&db, "TRUE").await, 2, "both rows landed");
    assert_eq!(
        channel
            .stored(
                &db,
                &format!("r.site_id = '{site}' AND r.parameter_id = '{param}' AND r.sensor_id IS NOT NULL")
            )
            .await,
        2,
        "the rows agree with the pairing the channel holds when they land"
    );
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT count(*) AS n FROM reprocessing_jobs WHERE trigger_type = 'pairing_backfill' \
                 AND trigger_id = '{}' AND status = 'completed'",
                channel.stream_id
            )
        )
        .await,
        1,
        "the slot reprocess ran over the rows"
    );
}

/// Scenario: a file is imported against a paired channel, and the channel is unpaired before the
/// import's job runs.
/// Expected behaviour: the rows land with no site, parameter, instrument, deployment or curve.
#[tokio::test]
#[serial]
async fn a_channel_unpaired_before_the_import_runs_stages_its_rows() {
    let (db, app, token) = setup().await;
    crate::common::stop_test_workers().await;
    let site = crate::common::SITE1_ID;
    let param =
        crate::common::e2e::create_parameter(&app, &token, "unpairedlater", "Unpaired later", "m")
            .await;
    crate::common::e2e::assign_site_parameter_minimal(&app, &token, site, &param).await;

    import_column(&app, &token, "unpairedlater").await;
    let channel = Channel::of(&db, site, &param).await;
    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{}/unpair", channel.stream_id),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "unpair it: {status} {text}");

    run_queued_jobs(&db).await;

    assert_eq!(channel.stored(&db, "TRUE").await, 2, "both rows landed");
    assert_eq!(
        channel
            .stored(
                &db,
                "r.site_id IS NULL AND r.parameter_id IS NULL AND r.sensor_id IS NULL \
                 AND r.deployment_id IS NULL AND r.calibration_id IS NULL"
            )
            .await,
        2,
        "rows on an unpaired channel are staged, not attributed"
    );
}
