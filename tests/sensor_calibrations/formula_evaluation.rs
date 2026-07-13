//! Tests for calibration formula evaluation edge cases.
//!
//! Run with: cargo test --test sensor_calibrations
//! Requires: DATABASE_URL pointing to a TimescaleDB instance.


use serial_test::serial;
use std::collections::HashMap;

// ============================================================================
// Unit tests for evaluate_formula (no DB needed)
// ============================================================================

#[test]
fn test_division_by_zero_produces_non_finite() {
    // meval returns Ok(Infinity) for division by zero.
    // The code at calibration.rs should reject non-finite results.
    let mut vars = HashMap::new();
    vars.insert("a".to_string(), 1.0);
    vars.insert("b".to_string(), 0.0);

    let result = river_db::routes::private::sensors::calibrations::service::evaluate_formula("a / b", &vars);
    // evaluate_formula returns Ok(Infinity) — the caller must check is_finite()
    assert!(result.is_ok(), "meval should not error on division by zero");
    let value = result.unwrap();
    assert!(
        !value.is_finite(),
        "division by zero should produce non-finite value (Infinity), got {value}"
    );
}

#[test]
fn test_missing_variable_returns_error() {
    let mut vars = HashMap::new();
    vars.insert("a".to_string(), 1.0);
    // "b" is missing

    let result = river_db::routes::private::sensors::calibrations::service::evaluate_formula("a + b", &vars);
    assert!(result.is_err(), "missing variable should produce an error");
}

#[test]
fn test_normal_formula_evaluation() {
    let mut vars = HashMap::new();
    vars.insert("a".to_string(), 3.0);
    vars.insert("b".to_string(), 4.0);

    let result = river_db::routes::private::sensors::calibrations::service::evaluate_formula("a * b + 2", &vars);
    assert!(result.is_ok());
    let value = result.unwrap();
    assert!((value - 14.0).abs() < 1e-10, "3*4+2 should be 14, got {value}");
}

// ============================================================================
// Integration test: derived parameter with division by zero
// ============================================================================

#[tokio::test]
#[serial]
async fn test_derived_parameter_skips_infinity() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    use sea_orm::{ConnectionTrait, Statement};

    let site_id = crate::common::SITE1_ID;
    let derived_param_id = "00000000-0000-4000-b000-000000000099";
    let derived_sp_id = "00000000-0000-4000-a000-000000000199";
    let derived_def_id = "00000000-0000-4000-c000-000000000001";
    let derived_stream_id = "00000000-0000-4000-c000-000000000010";
    let do_stream_id = "00000000-0000-4000-d000-000000000002";
    let temp_stream_id = "00000000-0000-4000-d000-000000000001";

    // Create a global parameter for the derived value
    exec(
        &db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
             VALUES ('{derived_param_id}', 'TempOverDO', 'Temp / DO', 'ratio', 'measurement')"
        ),
    )
    .await;

    // Create a derived parameter definition with a division formula
    exec(
        &db,
        &format!(
            "INSERT INTO derived_parameter_definitions (id, code, name, units, formula) \
             VALUES ('{derived_def_id}', 'TempOverDO', 'Temp / DO', 'ratio', 'temp / do_val')"
        ),
    )
    .await;

    // Create a site_parameter for the derived value
    exec(
        &db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type, is_active, is_derived, derived_definition_id, variable_mappings) \
             VALUES ('{derived_sp_id}', '{site_id}', '{derived_param_id}', 'TempOverDO', 'TempOverDO', true, true, '{derived_def_id}', \
             '{{\"temp\": \"{sp_temp}\", \"do_val\": \"{sp_do}\"}}'::jsonb)",
            sp_temp = crate::common::PARAM_S1_TEMP_ID,
            sp_do = crate::common::PARAM_S1_DO_ID,
        ),
    )
    .await;

    // Create a data stream for the derived readings
    crate::common::seed_data_stream(&db, derived_stream_id, "test", "derived_stream").await;
    // Pair it to the derived site_parameter
    exec(
        &db,
        &format!(
            "UPDATE data_streams SET site_parameter_id = '{derived_sp_id}', paired_at = NOW() WHERE id = '{derived_stream_id}'"
        ),
    )
    .await;

    // Insert a reading where DO = 0 (will cause division by zero in the formula)
    let time = "2025-01-15T00:00:00Z";
    // Overwrite the DO reading at this time with 0
    exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{do_stream_id}', '{site_id}', '{do_param}', '{time}', 0.0, 0) \
             ON CONFLICT (stream_id, time, replicate_index) DO UPDATE SET raw_value = 0.0, calibrated_value = NULL",
            do_param = crate::common::GLOBAL_PARAM_DO_ID,
        ),
    )
    .await;

    // Ensure temp reading exists at this time
    exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{temp_stream_id}', '{site_id}', '{temp_param}', '{time}', 10.0, 0) \
             ON CONFLICT (stream_id, time, replicate_index) DO UPDATE SET raw_value = 10.0, calibrated_value = NULL",
            temp_param = crate::common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await;

    // Recalculate derived parameters at this timestamp
    let site_uuid: uuid::Uuid = site_id.parse().unwrap();
    let time_dt: chrono::DateTime<chrono::Utc> = time.parse().unwrap();

    let result =
        river_db::routes::private::sensors::calibrations::service::recalculate_derived_at_timestamp(&db, site_uuid, time_dt)
            .await;
    assert!(result.is_ok(), "recalculate should not error: {result:?}");

    // Check that NO derived reading was written (Infinity should have been skipped)
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT raw_value FROM readings WHERE site_id = $1 AND parameter_id = $2 AND time = $3",
            [
                site_uuid.into(),
                derived_param_id.parse::<uuid::Uuid>().unwrap().into(),
                time_dt.into(),
            ],
        ))
        .await
        .unwrap();

    assert!(
        row.is_none(),
        "derived reading with Infinity result should NOT have been written to the database"
    );

    crate::common::cleanup_test_db(&db).await;
}

async fn exec(db: &sea_orm::DatabaseConnection, sql: &str) {
    use sea_orm::{ConnectionTrait, Statement};
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("SQL failed: {e}\nQuery: {sql}"));
}

// Calibration application formula (calibrated = slope * raw + intercept) and slope=0 rejection.

#[tokio::test]
async fn apply_calibration_formula_correctness() {
    // Test the actual formula against expected lab calibration results.
    // The code uses: calibrated = slope * raw + intercept
    let result = river_db::routes::private::sensors::calibrations::service::apply_calibration(10.0, 2.0, 5.0);
    assert_eq!(result, 25.0, "calibrated = 2.0 * 10.0 + 5.0 = 25.0");

    // Identity calibration (slope=1, intercept=0)
    let result = river_db::routes::private::sensors::calibrations::service::apply_calibration(42.0, 1.0, 0.0);
    assert_eq!(result, 42.0, "Identity calibration should return raw value");

    // Negative intercept
    let result = river_db::routes::private::sensors::calibrations::service::apply_calibration(100.0, 1.0, -273.15);
    assert!(
        (result - (-173.15)).abs() < 0.001,
        "Kelvin to Celsius: 100 - 273.15 = -173.15, got {result}"
    );
}

#[tokio::test]
#[serial]
async fn slope_zero_rejected_by_validation() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO projects (id, name, data_source) VALUES ('{pid}', 'Bug5 Project', 'test')",
            pid = crate::common::PROJECT_ID
        ),
    )
    .await;
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO sites (id, project_id, name) VALUES ('{sid}', '{pid}', 'Bug5 Site')",
            sid = crate::common::SITE1_ID,
            pid = crate::common::PROJECT_ID
        ),
    )
    .await;

    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
             VALUES ('{gid}', 'Temperature', 'Temperature', '°C', 'measurement')",
            gid = crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;

    let sensor_id = "00000000-0000-4000-e000-000000000001";
    crate::common::db::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, serial_number, manufacturer, model) \
             VALUES ('{sensor_id}', 'TEST-001', 'TestCo', 'T1')"
        ),
    )
    .await;

    let app = crate::common::build_test_app(db.clone());
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/sensor_calibrations",
        &serde_json::json!({
            "sensor_id": sensor_id,
            "slope": 0.0,
            "intercept": 5.0,
            "valid_from": "2025-01-01T00:00:00Z"
        }),
        &token,
    )
    .await;

    assert_eq!(
        status, 400,
        "slope=0 should be rejected but got {status}: {body}"
    );
}
