//! Delete operations on entities that have FK dependencies.
//! Verifies that deleting an entity with child data either succeeds
//! (via before_delete hooks or CASCADE) or returns a clear error.

use serial_test::serial;

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

// Scenario: a site stops measuring a parameter someone wants the slot gone for.
// Expected behaviour: the delete is refused while the slot holds readings, and the streams paired
// to it stay paired; retirement is the Active flag, which keeps the history attributed (Q160).
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

    let (status, body) =
        crate::common::delete_with_token(&app, &format!("/api/site_parameters/{sp_id}"), &token)
            .await;
    assert_eq!(status, 400, "a measured slot is refused: {body}");

    let still_paired = count(
        &db,
        &format!("SELECT count(*) AS c FROM data_streams WHERE site_parameter_id = '{sp_id}'"),
    )
    .await;
    assert_eq!(still_paired, stream_count, "the refusal changed nothing");

    let attributed = count(
        &db,
        &format!(
            "SELECT count(*) AS c FROM readings r JOIN site_parameters sp ON sp.id = '{sp_id}' \
             WHERE r.site_id = sp.site_id AND r.parameter_id = sp.parameter_id"
        ),
    )
    .await;
    assert!(attributed > 0, "the readings are still the slot's");

    let sp_exists = count(
        &db,
        &format!("SELECT count(*) AS c FROM site_parameters WHERE id = '{sp_id}'"),
    )
    .await;
    assert_eq!(sp_exists, 1, "the slot is still there");
}

// Scenario: a slot paired by mistake, before anything was measured through it.
// Expected behaviour: DELETE succeeds and the streams paired to it are released.
#[tokio::test]
#[serial]
async fn delete_unmeasured_site_parameter_unpairs_its_streams() {
    let (db, app, token) = setup().await;

    let (status, text) = crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE2_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_DEPTH_ID,
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create the slot: {status} {text}"
    );
    let created: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    let sp_id = created["id"].as_str().expect("id").to_string();

    let (rstatus, rtext) = crate::common::post_json_with_token(
        &app,
        "/api/streams/register",
        &serde_json::json!({
            "source_system": "test",
            "source_key": "unmeasured-slot-stream",
            "name": "Unmeasured slot stream",
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&rstatus),
        "register a stream: {rstatus} {rtext}"
    );
    let stream: serde_json::Value = serde_json::from_str(&rtext).expect("valid json");
    let stream_id = stream["id"].as_str().expect("stream id").to_string();

    let (pstatus, ptext) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &serde_json::json!({ "site_parameter_id": sp_id }),
        &token,
    )
    .await;
    assert!((200..300).contains(&pstatus), "pair it: {pstatus} {ptext}");

    let (dstatus, dtext) =
        crate::common::delete_with_token(&app, &format!("/api/site_parameters/{sp_id}"), &token)
            .await;
    assert!(
        (200..300).contains(&dstatus),
        "an unmeasured slot deletes: {dstatus} {dtext}"
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
    let calculation =
        crate::common::seed_formula_calculation(&db, &format!("{def_name}_set")).await;
    let (_, def) = crate::common::post_json_parse_with_token(
        &app,
        "/api/derived_parameters",
        &serde_json::json!({
            "code": def_name,
            "name": "Delete Test",
            "units": "mg/L",
            "formula": "Dissolved_O2 * 0.032",
            "tool_script_id": calculation,
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
        "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('00000000-0000-4000-d000-000000000096', 'test', 'del-curve', 'x', true)",
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

// Scenario: a stream that carries readings is deleted directly in the database.
// Expected behaviour: the FK refuses it, because a reading resolves its origin through the stream
// it names and a stream removed underneath it would take that record with it.
#[tokio::test]
#[serial]
async fn delete_stream_with_readings_is_refused() {
    use sea_orm::{ConnectionTrait, Statement};

    let (db, _app, _token) = setup().await;

    let stream_id = "00000000-0000-4000-d000-000000000097";
    let sensor_id = uuid::Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!("INSERT INTO sensors (id, serial_number) VALUES ('{sensor_id}', 'DEL-TEST-003')"),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, sensor_id, \
                                       is_active) \
             VALUES ('{stream_id}', 'test', 'del-stream', 'x', '{sensor_id}', true)"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, time, replicate_index, raw_value, sensor_id) \
             VALUES ('{stream_id}', '2025-06-01T00:00:00Z', 0, 10.0, '{sensor_id}')"
        ),
    )
    .await;

    let deleted = db
        .execute_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("DELETE FROM data_streams WHERE id = '{stream_id}'"),
        ))
        .await;
    let refusal =
        deleted.expect_err("deleting a stream that carries readings must be refused, not cascade");
    assert!(
        refusal.to_string().contains("readings_stream_id_fkey"),
        "the refusal must come from the readings FK, got: {refusal}"
    );

    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) AS c FROM readings WHERE stream_id = '{stream_id}'")
        )
        .await,
        1,
        "the reading is still stored"
    );
}

async fn count(db: &sea_orm::DatabaseConnection, sql: &str) -> i64 {
    use sea_orm::{ConnectionTrait, Statement};
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .ok()
    .flatten()
    .and_then(|r| r.try_get::<i64>("", "c").ok())
    .unwrap_or(0)
}
