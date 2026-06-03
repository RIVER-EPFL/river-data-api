//! CSV import endpoint: column resolution (explicit mapping > public name > alias > catalog),
//! skip-derived, dry-run preview, idempotency, and rejection of unresolvable files.
//!
//! Run with: cargo test --test csv_import_test

mod common;

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());
    (db, app, token)
}

const CSV: &str = "DateTime,DOmgL,DOuM,WaterTempdegC\n\
2025-02-01 00:00:00,8.0,250,12.0\n\
2025-02-01 00:10:00,9.6,300,12.5\n";

/// Create the DOmgL derived parameter, assign it to SITE1, and expose DOuM/WaterTempdegC/DOmgL
/// publicly under the client's names. Returns the derived output parameter UUID.
async fn configure_derived_and_exposure(
    db: &DatabaseConnection,
    app: &axum::Router,
    token: &str,
) -> Uuid {
    let derived_name = format!("DOmgL_{}", Uuid::new_v4().simple());
    let (_s, def) = common::post_json_parse_with_token(
        app,
        "/api/derived_parameters",
        &serde_json::json!({
            "code": derived_name, "name": "DO mg/L", "units": "mg/L",
            "formula": "Dissolved_O2 * 0.032",
        }),
        token,
    )
    .await;
    let derived_def_id = def["id"].as_str().unwrap().to_string();
    let output_parameter_id = def["output_parameter_id"].as_str().unwrap().to_string();

    common::post_json_with_token(
        app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": common::SITE1_ID, "parameter_id": output_parameter_id, "name": derived_name,
            "sensor_type": "derived", "is_derived": true, "derived_definition_id": derived_def_id,
            "display_units": "mg/L",
        }),
        token,
    )
    .await;

    common::exec(
        db,
        &format!(
            "UPDATE parameters SET aliases = ARRAY['DOuM'] WHERE id = '{}'",
            common::GLOBAL_PARAM_DO_ID
        ),
    )
    .await;
    common::exec(
        db,
        &format!(
            "UPDATE parameters SET aliases = ARRAY['WaterTempdegC'] WHERE id = '{}'",
            common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    common::exec(
        db,
        &format!(
            "UPDATE parameters SET aliases = ARRAY['DOmgL'] WHERE id = '{}'",
            output_parameter_id
        ),
    )
    .await;

    Uuid::parse_str(&output_parameter_id).unwrap()
}

async fn import(
    app: &axum::Router,
    token: &str,
    body: &serde_json::Value,
) -> (u16, serde_json::Value) {
    common::post_json_parse_with_token(app, "/api/readings/import_csv", body, token).await
}

#[tokio::test]
#[serial]
async fn test_csv_import_dry_run_then_write_skips_derived_and_recomputes() {
    let (db, app, token) = setup().await;
    let derived_param = configure_derived_and_exposure(&db, &app, &token).await;

    // Dry run: report the plan, write nothing.
    let (status, plan) = import(
        &app,
        &token,
        &serde_json::json!({"site": common::SITE1_ID, "csv": CSV, "dry_run": true}),
    )
    .await;
    assert_eq!(status, 200, "dry_run ({status}): {plan}");
    assert!(plan["dry_run"].as_bool().unwrap());
    assert_eq!(plan["row_count"].as_u64().unwrap(), 2);
    assert_eq!(plan["inserted_total"].as_u64().unwrap(), 0);
    assert_eq!(plan["mapped_columns"]["DOuM"], "Dissolved_O2");
    assert_eq!(plan["mapped_columns"]["WaterTempdegC"], "DO_Temperature");
    let skipped: Vec<String> = plan["skipped_columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(skipped.contains(&"DOmgL".to_string()), "DOmgL must be skipped: {plan}");
    assert_eq!(
        count_readings(&db, common::GLOBAL_PARAM_DO_ID, "2025-02-01T00:00:00Z").await,
        0,
        "dry_run must not write"
    );

    // Real import.
    let (status, resp) = import(
        &app,
        &token,
        &serde_json::json!({"site": common::SITE1_ID, "csv": CSV}),
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");
    assert_eq!(resp["inserted_total"].as_u64().unwrap(), 4);
    assert!(resp["derived_job_id"].is_string(), "expected a background derived job: {resp}");

    // Insert runs in a background job; poll until the reading appears.
    assert_eq!(
        poll_scalar(&db, common::GLOBAL_PARAM_DO_ID, "2025-02-01T00:00:00Z", "raw_value", 10).await,
        Some(250.0),
        "background insert should write raw_value within 10s"
    );

    // Idempotent re-import: nothing inserted, no background job started.
    let (_s, again) = import(
        &app,
        &token,
        &serde_json::json!({"site": common::SITE1_ID, "csv": CSV}),
    )
    .await;
    assert_eq!(again["inserted_total"].as_u64().unwrap(), 0, "re-import must insert nothing");
    assert!(again["derived_job_id"].is_null(), "no-op re-import should not start a job");
}

#[tokio::test]
#[serial]
async fn test_csv_import_explicit_mapping_overrides_and_skips() {
    let (db, app, token) = setup().await;

    // Headers that don't match any name; map them explicitly, and skip one with null.
    let csv = "DateTime,oxygen,temp_c,junk\n2025-02-02 00:00:00,250,12.0,999\n";
    let (status, resp) = import(
        &app,
        &token,
        &serde_json::json!({
            "site": common::SITE1_ID,
            "csv": csv,
            "mapping": { "oxygen": "Dissolved_O2", "temp_c": "DO_Temperature", "junk": null },
        }),
    )
    .await;
    assert_eq!(status, 200, "explicit mapping ({status}): {resp}");
    assert_eq!(resp["mapped_columns"]["oxygen"], "Dissolved_O2");
    assert_eq!(resp["mapped_columns"]["temp_c"], "DO_Temperature");
    let skipped: Vec<String> = resp["skipped_columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(skipped.contains(&"junk".to_string()), "junk must be skipped: {resp}");
    assert_eq!(resp["inserted_total"].as_u64().unwrap(), 2);
    assert_eq!(
        poll_scalar(&db, common::GLOBAL_PARAM_DO_ID, "2025-02-02T00:00:00Z", "raw_value", 10).await,
        Some(250.0),
        "background insert should write raw_value within 10s"
    );
}

#[tokio::test]
#[serial]
async fn test_csv_import_rejects_when_no_columns_resolve() {
    let (_db, app, token) = setup().await;
    let csv = "DateTime,Nonsense,AlsoUnknown\n2025-02-01 00:00:00,1,2\n";
    let (status, _resp) = import(
        &app,
        &token,
        &serde_json::json!({"site": common::SITE1_ID, "csv": csv}),
    )
    .await;
    assert_eq!(status, 400, "unrecognised columns should be a 400");
}

#[tokio::test]
#[serial]
async fn test_csv_import_collects_row_errors_and_counts_duplicates() {
    let (db, app, token) = setup().await;
    // Row 2 good; row 3 has a bad DateTime; row 4 has a non-numeric Dissolved_O2 (its temperature
    // is still good). Headers match the catalog names directly.
    let csv = "DateTime,Dissolved_O2,DO_Temperature\n\
2025-04-01 00:00:00,250,12.0\n\
not-a-date,260,13.0\n\
2025-04-01 00:10:00,abc,13.5\n";

    let (status, resp) = import(
        &app,
        &token,
        &serde_json::json!({"site": common::SITE1_ID, "csv": csv}),
    )
    .await;
    assert_eq!(status, 200, "bad rows must not fail the whole import ({status}): {resp}");
    assert_eq!(resp["inserted_total"].as_u64().unwrap(), 3, "2 good cells in row 2 + 1 in row 4: {resp}");
    assert_eq!(resp["duplicates"].as_u64().unwrap(), 0, "{resp}");
    assert_eq!(resp["error_count"].as_u64().unwrap(), 2, "{resp}");
    let errors = resp["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 2);
    let msgs = errors
        .iter()
        .map(|e| e["message"].as_str().unwrap())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(msgs.contains("DateTime"), "should flag the bad timestamp: {msgs}");
    assert!(msgs.contains("abc"), "should flag the non-numeric value: {msgs}");

    // Wait for the background insert to land before re-importing.
    assert!(
        poll_scalar(&db, common::GLOBAL_PARAM_DO_ID, "2025-04-01T00:00:00Z", "raw_value", 10).await.is_some(),
        "background insert should write within 10s"
    );

    // Re-import the same file: good rows are now duplicates (skipped, not errored); bad rows still error.
    let (_s, again) = import(
        &app,
        &token,
        &serde_json::json!({"site": common::SITE1_ID, "csv": csv}),
    )
    .await;
    assert_eq!(again["inserted_total"].as_u64().unwrap(), 0, "re-import inserts nothing: {again}");
    assert_eq!(again["duplicates"].as_u64().unwrap(), 3, "re-import counts duplicates: {again}");
    assert_eq!(again["error_count"].as_u64().unwrap(), 2, "re-import still reports errors: {again}");
    assert!(again["derived_job_id"].is_null(), "no-op re-import starts no job");
}

async fn count_readings(db: &DatabaseConnection, parameter_id: &str, time_rfc3339: &str) -> i64 {
    let param = Uuid::parse_str(parameter_id).unwrap();
    let time: chrono::DateTime<chrono::Utc> = time_rfc3339.parse().unwrap();
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*) AS n FROM readings WHERE parameter_id = $1 AND time = $2",
            [param.into(), time.into()],
        ))
        .await
        .ok()
        .flatten();
    row.and_then(|r| r.try_get::<i64>("", "n").ok()).unwrap_or(0)
}

async fn poll_scalar(
    db: &DatabaseConnection,
    parameter_id: &str,
    time_rfc3339: &str,
    column: &str,
    max_secs: u64,
) -> Option<f64> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(max_secs);
    loop {
        if let Some(v) = scalar(db, parameter_id, time_rfc3339, column).await {
            return Some(v);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

async fn scalar(
    db: &DatabaseConnection,
    parameter_id: &str,
    time_rfc3339: &str,
    column: &str,
) -> Option<f64> {
    let param = Uuid::parse_str(parameter_id).unwrap();
    let time: chrono::DateTime<chrono::Utc> = time_rfc3339.parse().unwrap();
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &format!("SELECT {column} AS v FROM readings WHERE parameter_id = $1 AND time = $2 LIMIT 1"),
            [param.into(), time.into()],
        ))
        .await
        .ok()
        .flatten()?;
    row.try_get::<f64>("", "v").ok()
}
