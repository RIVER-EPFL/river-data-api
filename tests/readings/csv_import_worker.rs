//! CSV import runs on the worker pool: the handler stages the parsed rows and enqueues a
//! `csv_import` job; a worker claims it, inserts the readings, recomputes derived values, and the
//! staging rows are deleted. This is the durability flip, no inline `spawn_tracked_job_ctx` whose
//! in-memory `Vec` would strand on a dead replica.
//!
//! Run with: cargo test --test readings

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

const CSV: &str = "DateTime,Dissolved_O2,DO_Temperature\n\
2025-06-01 00:00:00,250,12.0\n\
2025-06-01 00:10:00,300,12.5\n";

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
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

    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT status FROM reprocessing_jobs WHERE id = '{job_id}'"),
        ))
        .await
        .unwrap()
        .unwrap();
    let job_status: String = row.try_get("", "status").unwrap();
    assert_eq!(
        job_status, "completed",
        "the csv_import job reaches completed"
    );
}

const CSV_DUP_TS: &str = "DateTime,Dissolved_O2,DO_Temperature\n\
2025-06-01 00:00:00,250,12.0\n\
2025-06-01 00:00:00,260,12.5\n";

async fn scalar_f64(db: &DatabaseConnection, sql: &str) -> f64 {
    db.query_one(Statement::from_string(
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
    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
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
        .query_one(Statement::from_string(
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
    let (_s, def) = crate::common::post_json_parse_with_token(
        &app,
        "/api/derived_parameters",
        &serde_json::json!({
            "code": derived_name, "name": "DO mg/L", "units": "mg/L",
            "formula": "Dissolved_O2 * 0.032",
        }),
        &token,
    )
    .await;
    let derived_def_id = def["id"].as_str().unwrap().to_string();
    let output_parameter_id = def["output_parameter_id"].as_str().unwrap().to_string();
    crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID, "parameter_id": output_parameter_id, "name": derived_name,
            "sensor_type": "derived", "is_derived": true, "derived_definition_id": derived_def_id,
            "display_units": "mg/L",
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

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
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

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
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
    assert_eq!(readings_after, 3, "no renumbered duplicates on re-import");

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

/// Expected behaviour: a file declared `spot` is a set of collection events, so each row is a grab
/// with its own `samples` row even when it was measured once. Views that read grabs, the
/// sensor-vs-grab export among them, join through `samples`.
#[tokio::test]
#[serial]
async fn declared_spot_import_gives_each_grab_its_sample_row() {
    let (db, app, token) = setup().await;

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": CSV_SINGLE_GRABS,
            "measurement_type": "spot",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");

    let samples = poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM samples \
             WHERE site_id = '{}' AND collected_at >= '2025-06-03T00:00:00Z'",
            crate::common::SITE1_ID
        ),
        2,
        10,
    )
    .await;
    assert_eq!(samples, 2, "one sample per single-row grab");

    let stamped = scalar_i64(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE site_id = '{}' AND time >= '2025-06-03T00:00:00Z' AND sample_id IS NOT NULL",
            crate::common::SITE1_ID
        ),
    )
    .await;
    assert_eq!(stamped, 2, "each grab references its sample");

    let mean = scalar_f64(
        &db,
        &format!(
            "SELECT mean AS v FROM samples \
             WHERE site_id = '{}' AND collected_at = '2025-06-03T09:00:00Z'",
            crate::common::SITE1_ID
        ),
    )
    .await;
    assert!(
        (mean - 140.0).abs() < 1e-9,
        "the sample statistic is the single measurement: got {mean}"
    );
}

const CSV_CONTINUOUS_DUP: &str = "DateTime,Dissolved_O2\n\
2025-06-04 00:00:00,200\n\
2025-06-04 00:00:00,210\n";

/// Expected behaviour: two logger points sharing a timestamp are a malformed file, not a sampling
/// event. They are still stored as replicates, but no `samples` row is invented around them.
#[tokio::test]
#[serial]
async fn continuous_rows_sharing_a_timestamp_form_no_sample() {
    let (db, app, token) = setup().await;

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": crate::common::SITE1_ID, "csv": CSV_CONTINUOUS_DUP }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");

    let readings = poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE site_id = '{}' AND time = '2025-06-04T00:00:00Z'",
            crate::common::SITE1_ID
        ),
        2,
        10,
    )
    .await;
    assert_eq!(readings, 2, "both rows are stored as replicates");

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
        .query_one(Statement::from_string(
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

async fn import_grab_csv(app: &axum::Router, token: &str) {
    let (status, resp) = crate::common::post_json_parse_with_token(
        app,
        "/api/readings/import_csv",
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": CSV_GRAB,
            "measurement_type": "spot",
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
        .query_one(Statement::from_string(
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
    let (status, resp) =
        crate::common::post_json_parse_with_token(app, "/api/readings/import_csv", body, token)
            .await;
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
        &serde_json::json!({ "site": crate::common::SITE1_ID, "csv": CSV_DO_AT }),
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
