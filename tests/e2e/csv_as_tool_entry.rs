//! S4, CSV import as data entry (story catalog: ../archived-documentation/PLAN.md).
//!
//! Scenario: a member imports a result sheet. Already-processed values are stored as served: no
//! calibration is stamped onto them and nothing recomputes them, because a stored calibration id
//! claims the row's `raw_value` is uncorrected input (ADR 0003). A declared-raw import is the
//! other arm: the covering calibration is stamped and applied.
//!
//! S4a is here too: a CSV of raw tool inputs runs the named tool once per row, with a `tool_runs`
//! row and the same server-built provenance blob a typed entry gets.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

/// The site parameter the doc tool's replicates land on. A tool save writes onto the slots the
/// site carries and mints none (Q98), so the story declares its slot before it imports.
async fn configure_doc_slot(db: &DatabaseConnection) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type, is_active) \
             VALUES (gen_random_uuid(), '{site}', '00000000-0000-4000-b000-0000000000d0', 'DOC', \
                     'lab', true)",
            site = crate::common::SITE1_ID,
        ),
    )
    .await;
}

/// Poll until the import worker has landed `want` readings at SITE1.
async fn poll_readings(db: &DatabaseConnection, time: &str, want: i64) -> i64 {
    let sql = format!(
        "SELECT count(*) AS n FROM readings WHERE site_id = '{}' AND time = '{time}'",
        crate::common::SITE1_ID
    );
    for _ in 0..100 {
        let row = db
            .query_one_raw(Statement::from_string(
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

async fn imported_row(
    db: &DatabaseConnection,
    time: &str,
) -> (f64, Option<f64>, Option<uuid::Uuid>) {
    let row = db
        .query_one_raw(Statement::from_string(
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
/// untouched (no calibration id, no recomputation), although the very same rows imported as
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
    assert!(
        calibration_id.is_some(),
        "the applied calibration is recorded"
    );
}

/// Expected behaviour: a request field the server does not know is refused by name, never
/// silently dropped: the guard that keeps a version skew from eating a declaration.
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

/// S4a: importing raw tool inputs runs the tool over each row: same write path, same `tool_runs`
/// row and server-built provenance blob as a typed entry, source recorded as the import.
#[tokio::test]
#[serial]
async fn an_import_of_raw_inputs_runs_the_tool_and_carries_its_provenance() {
    if !crate::common::profile::Service::ToolsRunner
        .require("an_import_of_raw_inputs_runs_the_tool_and_carries_its_provenance")
        .await
    {
        return;
    }
    let (db, app, token) = setup().await;
    // The catalog parameter the doc tool's replicates are readings of, under the code its manifest
    // names, on a slot the site carries: a tool save mints none (Q98).
    crate::common::exec(
        &db,
        "INSERT INTO parameters (id, code, name, category) \
         VALUES ('00000000-0000-4000-b000-0000000000d0', 'DOC_ppb', 'DOC', 'measurement')",
    )
    .await;
    let doc_param = "00000000-0000-4000-b000-0000000000d0";
    configure_doc_slot(&db).await;

    let csv = "DateTime,DOC_rep_1,DOC_rep_2,DOC_rep_3,DOC_notes\n\
               2025-06-01 10:00:00,120,125,118,\n\
               2025-06-02 10:00:00,130,131,129,\n";

    // The plan first: columns map to tool inputs, the stray column is named, nothing runs.
    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": csv,
            "tool": "doc",
            "dry_run": true,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "plan ({status}): {plan}");
    assert_eq!(plan["mapped_columns"]["DOC_rep_1"], "DOC");
    assert_eq!(plan["unmapped_columns"][0], "DOC_notes", "{plan}");
    assert_eq!(plan["row_count"], 2);
    assert_eq!(plan["tool_runs_created"], 0);

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": csv,
            "tool": "doc",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");
    assert_eq!(resp["tool_runs_created"], 2, "{resp}");
    assert_eq!(resp["error_count"], 0, "{resp}");

    // One run per row, minted by the import path.
    let runs = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COUNT(*)::bigint AS n FROM tool_runs \
             WHERE tool_name = 'doc' AND source = 'csv_import'"
                .to_string(),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "n")
        .unwrap();
    assert_eq!(runs, 2);

    // Each row's replicates went through the grab write path as readings of the DOC parameter:
    // each carrying the server-built blob, source csv_import, every replicate attached to a
    // collection event, the mean derived by the database.
    for (time, expected) in [
        ("2025-06-01T10:00:00Z", (120.0 + 125.0 + 118.0) / 3.0),
        ("2025-06-02T10:00:00Z", 130.0),
    ] {
        let row = db
            .query_one_raw(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT r.provenance ->> 'source' AS source, r.provenance ->> 'tool' AS tool, \
                            COALESCE(s.mean, COALESCE(r.calibrated_value, r.raw_value)) AS mean, \
                            (SELECT COUNT(*)::bigint FROM readings r2 \
                              WHERE r2.site_id = r.site_id AND r2.parameter_id = r.parameter_id \
                                AND r2.time = r.time AND r2.collection_event_id IS NOT NULL) \
                              AS attached \
                     FROM readings r LEFT JOIN samples s ON s.id = r.sample_id \
                     WHERE r.site_id = '{}' AND r.parameter_id = '{doc_param}' \
                       AND r.time = '{time}' \
                     ORDER BY r.replicate_index LIMIT 1",
                    crate::common::SITE1_ID
                ),
            ))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("the saved reading at {time} exists"));
        assert_eq!(row.try_get::<String>("", "source").unwrap(), "csv_import");
        assert_eq!(row.try_get::<String>("", "tool").unwrap(), "doc");
        let mean = row.try_get::<Option<f64>>("", "mean").unwrap().unwrap();
        assert!(
            (mean - expected).abs() < 1e-9,
            "served {mean}, portal math {expected}"
        );
        assert_eq!(row.try_get::<i64>("", "attached").unwrap(), 3);
    }

    // The import wrote onto the slot the site declares and minted none beside it (Q98).
    let slots = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT count(*)::bigint AS n FROM site_parameters \
                 WHERE site_id = '{}' AND parameter_id = '{doc_param}'",
                crate::common::SITE1_ID
            ),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(slots.try_get::<i64>("", "n").unwrap(), 1);
}

#[tokio::test]
#[serial]
async fn an_import_naming_a_curve_saves_the_replicates_it_corrected_with_it() {
    if !crate::common::profile::Service::ToolsRunner
        .require("an_import_naming_a_curve_saves_the_replicates_it_corrected_with_it")
        .await
    {
        return;
    }
    let (db, app, token) = setup().await;
    // The catalog parameter the doc tool's replicates are readings of, under the code its manifest
    // names, on a slot the site carries: a tool save mints none (Q98).
    crate::common::exec(
        &db,
        "INSERT INTO parameters (id, code, name, category) \
         VALUES ('00000000-0000-4000-b000-0000000000d0', 'DOC_ppb', 'DOC', 'measurement')",
    )
    .await;
    let doc_param = "00000000-0000-4000-b000-0000000000d0";
    configure_doc_slot(&db).await;
    let analyser =
        crate::common::sensor_lifecycle::create_sensor_without_curve(&db, "TOC analyser").await;
    let mut curves = Vec::new();
    for (name, slope, intercept) in [("Plate A", 2.0, 1.0), ("Plate B", 3.0, -1.0)] {
        let (status, body) = crate::common::post_json_parse_with_token(
            &app,
            "/api/standard_curves",
            &serde_json::json!({
                "sensor_id": analyser, "name": name, "slope": slope, "intercept": intercept,
            }),
            &token,
        )
        .await;
        assert!(
            (200..300).contains(&status),
            "curve {name} ({status}): {body}"
        );
        curves.push(body["id"].as_str().unwrap().parse::<Uuid>().unwrap());
    }
    let (plate_a, plate_b) = (curves[0], curves[1]);

    // Row 1 takes the request's curve, row 2 names its own in the slot's column.
    let csv = format!(
        "DateTime,DOC_rep_1,DOC_rep_2,std_curve\n\
         2025-06-01 10:00:00,120,125,\n\
         2025-06-02 10:00:00,130,131,{plate_b}\n"
    );
    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": csv,
            "tool": "doc",
            "curves": { "std_curve": plate_a },
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");
    assert_eq!(resp["tool_runs_created"], 2, "{resp}");
    assert_eq!(resp["error_count"], 0, "{resp}");

    // Each replicate is stored raw, attributed to the curve's instrument, carrying the curve the
    // run applied, and served corrected by it; the run recorded the curve it consumed.
    for (time, curve, raw, expected) in [
        ("2025-06-01T10:00:00Z", plate_a, 120.0, 2.0 * 120.0 + 1.0),
        ("2025-06-02T10:00:00Z", plate_b, 130.0, 3.0 * 130.0 - 1.0),
    ] {
        let row = db
            .query_one_raw(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT r.raw_value, r.calibrated_value, r.standard_curve_id, r.sensor_id, \
                            s.mean, \
                            (SELECT jsonb_array_length(t.curves) FROM tool_runs t \
                              WHERE t.id = (r.provenance ->> 'run_id')::uuid) AS run_curves \
                     FROM readings r LEFT JOIN samples s ON s.id = r.sample_id \
                     WHERE r.site_id = '{}' AND r.parameter_id = '{doc_param}' \
                       AND r.time = '{time}' AND r.replicate_index = 0",
                    crate::common::SITE1_ID
                ),
            ))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("the saved replicate at {time} exists"));
        assert_eq!(row.try_get::<f64>("", "raw_value").unwrap(), raw);
        assert_eq!(
            row.try_get::<Option<Uuid>>("", "standard_curve_id")
                .unwrap(),
            Some(curve)
        );
        assert_eq!(
            row.try_get::<Option<Uuid>>("", "sensor_id").unwrap(),
            Some(analyser)
        );
        let corrected = row
            .try_get::<Option<f64>>("", "calibrated_value")
            .unwrap()
            .unwrap();
        assert!(
            (corrected - expected).abs() < 1e-9,
            "stored {corrected}, curve {expected}"
        );
        let mean = row.try_get::<Option<f64>>("", "mean").unwrap().unwrap();
        assert!(
            mean > expected,
            "the served mean {mean} is over corrected replicates"
        );
        assert_eq!(
            row.try_get::<Option<i32>>("", "run_curves").unwrap(),
            Some(1)
        );
    }
}
