//! Delete operations on entities that have FK dependencies.
//! Verifies that deleting an entity with child data either succeeds
//! (via before_delete hooks or CASCADE) or returns a clear error.

use serial_test::serial;

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

// Scenario: site_parameter has paired data_streams + readings.
// Expected behaviour: DELETE succeeds, streams are unpaired (site_parameter_id set to NULL).
#[tokio::test]
#[serial]
async fn delete_site_parameter_with_paired_streams() {
    let (db, app, token) = setup().await;

    let sp_id = crate::common::PARAM_S1_DO_ID;

    let stream_count = count(
        &db,
        &format!("SELECT count(*) AS c FROM data_streams WHERE site_parameter_id = '{sp_id}'"),
    )
    .await;
    assert!(
        stream_count > 0,
        "site_parameter should have paired streams"
    );

    let (status, _) =
        crate::common::delete_with_token(&app, &format!("/api/site_parameters/{sp_id}"), &token)
            .await;
    assert!(
        status == 200 || status == 204,
        "DELETE should succeed, got {status}"
    );

    let unpaired = count(
        &db,
        &format!("SELECT count(*) AS c FROM data_streams WHERE site_parameter_id = '{sp_id}'"),
    )
    .await;
    assert_eq!(unpaired, 0, "streams should be unpaired after delete");

    let sp_exists = count(
        &db,
        &format!("SELECT count(*) AS c FROM site_parameters WHERE id = '{sp_id}'"),
    )
    .await;
    assert_eq!(sp_exists, 0, "site_parameter should be deleted");
}

// Scenario: derived_parameter_definition has sources and site_parameters referencing it.
// Expected behaviour: DELETE should handle FK dependencies.
#[tokio::test]
#[serial]
async fn delete_derived_definition_with_sources() {
    let (db, app, token) = setup().await;

    let def_name = format!("test_del_{}", uuid::Uuid::new_v4().simple());
    let (_, def) = crate::common::post_json_parse_with_token(
        &app,
        "/api/derived_parameters",
        &serde_json::json!({
            "code": def_name,
            "name": "Delete Test",
            "units": "mg/L",
            "formula": "Dissolved_O2 * 0.032",
        }),
        &token,
    )
    .await;
    let def_id = def["id"].as_str().unwrap();

    let source_count = count(
        &db,
        &format!("SELECT count(*) AS c FROM derived_parameter_sources WHERE derived_definition_id = '{def_id}'"),
    )
    .await;
    assert!(
        source_count > 0,
        "definition should have sources from formula"
    );

    let (status, _) = crate::common::delete_with_token(
        &app,
        &format!("/api/derived_parameters/{def_id}"),
        &token,
    )
    .await;
    assert!(
        status == 200 || status == 204,
        "DELETE derived definition should succeed, got {status}"
    );

    let remaining = count(
        &db,
        &format!("SELECT count(*) AS c FROM derived_parameter_sources WHERE derived_definition_id = '{def_id}'"),
    )
    .await;
    assert_eq!(remaining, 0, "sources should be cleaned up");
}

// Scenario: sensor has calibrations.
// Expected behaviour: DELETE should handle FK dependencies.
#[tokio::test]
#[serial]
async fn delete_sensor_with_calibrations() {
    let (db, app, token) = setup().await;

    let sensor_id = "00000000-0000-4000-d000-000000000099";
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, serial_number, name, manufacturer, model) \
             VALUES ('{sensor_id}', 'DEL-TEST-001', 'Delete Test', 'Test', 'T1')"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensor_calibrations (id, sensor_id, slope, intercept, valid_from) \
             VALUES (gen_random_uuid(), '{sensor_id}', 1.0, 0.0, '2025-01-01')"
        ),
    )
    .await;

    let cal_count = count(
        &db,
        &format!("SELECT count(*) AS c FROM sensor_calibrations WHERE sensor_id = '{sensor_id}'"),
    )
    .await;
    assert!(cal_count > 0, "sensor should have a calibration");

    let (status, body) =
        crate::common::delete_with_token(&app, &format!("/api/sensors/{sensor_id}"), &token).await;
    assert_eq!(
        status, 400,
        "a sensor holding calibrations refuses deletion with a stated error, got {status}: {body}"
    );
    assert!(
        body.contains("calibrations"),
        "the refusal names what blocks it: {body}"
    );

    let remaining = count(
        &db,
        &format!("SELECT count(*) AS c FROM sensor_calibrations WHERE sensor_id = '{sensor_id}'"),
    )
    .await;
    assert_eq!(remaining, 1, "nothing is deleted by a refused delete");
}

// Scenario: a sensor whose standard curve corrected published grabs.
// Expected behaviour: the delete is refused with a stated 400, never an FK-violation 500, and the
// curve and readings stay.
#[tokio::test]
#[serial]
async fn delete_sensor_with_referenced_standard_curve_is_refused() {
    let (db, app, token) = setup().await;

    let sensor_id = "00000000-0000-4000-d000-000000000098";
    let curve_id = "00000000-0000-4000-d000-000000000097";
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, serial_number, name, manufacturer, model) \
             VALUES ('{sensor_id}', 'DEL-TEST-002', 'Delete Test Curves', 'Test', 'T1')"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, name, slope, intercept) \
             VALUES ('{curve_id}', '{sensor_id}', 'Plate D', 2.0, 1.0)"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('00000000-0000-4000-d000-000000000096', 'test', 'del-curve', 'x', true)"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, time, replicate_index, raw_value, calibrated_value, \
                                   standard_curve_id, measurement_type) \
             VALUES ('00000000-0000-4000-d000-000000000096', '2025-06-01T00:00:00Z', 0, 10.0, \
                     21.0, '{curve_id}', 'spot')"
        ),
    )
    .await;

    let (status, body) =
        crate::common::delete_with_token(&app, &format!("/api/sensors/{sensor_id}"), &token).await;
    assert_eq!(
        status, 400,
        "a sensor whose curve corrected readings refuses deletion, got {status}: {body}"
    );

    let curve_remains = count(
        &db,
        &format!("SELECT count(*) AS c FROM standard_curves WHERE id = '{curve_id}'"),
    )
    .await;
    assert_eq!(curve_remains, 1, "the curve is untouched");
}

// Scenario: project has sites, which have site_parameters, readings, etc.
// Expected behaviour: DELETE should either cascade or return a clear error.
#[tokio::test]
#[serial]
async fn delete_project_with_sites_returns_error() {
    let (_db, app, token) = setup().await;

    let (status, body) = crate::common::delete_with_token(
        &app,
        &format!("/api/projects/{}", crate::common::PROJECT_ID),
        &token,
    )
    .await;

    assert!(
        status == 409 || status == 400 || status == 500,
        "deleting project with sites should fail: {status} {body}"
    );
}

// Scenario: site has site_parameters and readings.
// Expected behaviour: DELETE should either cascade or return a clear error.
#[tokio::test]
#[serial]
async fn delete_site_with_data_returns_error() {
    let (_db, app, token) = setup().await;

    let (status, body) = crate::common::delete_with_token(
        &app,
        &format!("/api/sites/{}", crate::common::SITE1_ID),
        &token,
    )
    .await;

    assert!(
        status == 409 || status == 400 || status == 500,
        "deleting site with data should fail: {status} {body}"
    );
}

async fn count(db: &sea_orm::DatabaseConnection, sql: &str) -> i64 {
    use sea_orm::{ConnectionTrait, Statement};
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .ok()
    .flatten()
    .and_then(|r| r.try_get::<i64>("", "c").ok())
    .unwrap_or(0)
}
