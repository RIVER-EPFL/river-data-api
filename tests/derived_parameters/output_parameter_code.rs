//! Scenario: an author fixes the code of a calculation after saving it, and the catalog parameter
//! the calculation publishes carries that code.
//!
//! Expected behaviour: the rename reaches the catalog while the code is unpublished, and is
//! refused once readings are stored under it or a project exposes it (Q183). A first save whose
//! code already belongs to a parameter no calculation produces is refused rather than adopting it
//! (Q191).
//!
//! Run: cargo test --test derived_parameters output_parameter_code -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

async fn define(
    db: &DatabaseConnection,
    app: &axum::Router,
    token: &str,
    code: &str,
) -> serde_json::Value {
    let calculation = crate::common::seed_formula_calculation(db, &format!("{code}_set")).await;
    let body = serde_json::json!({
        "code": code,
        "name": "Specific UV absorbance",
        "units": "L/mg/m",
        "formula": "Turbidity * 2",
        "description": "a254 over DOC",
        "tool_script_id": calculation,
    });
    let (status, text) =
        crate::common::post_json_with_token(app, "/api/derived_parameters", &body, token).await;
    assert!((200..300).contains(&status), "define ({status}): {text}");
    serde_json::from_str(&text).expect("the created calculation")
}

/// The calculation's versions, in order, as the round trip must leave them.
async fn versions(db: &DatabaseConnection, definition: Uuid) -> String {
    crate::common::e2e::scalar(
        db,
        &format!(
            "SELECT COALESCE(string_agg(v.version_no::text, ',' ORDER BY v.version_no), 'none') \
               FROM tool_script_versions v \
               JOIN calculation_formulas d ON d.tool_script_id = v.tool_script_id \
              WHERE d.id = '{definition}'"
        ),
    )
    .await
}

async fn rename(app: &axum::Router, token: &str, id: &str, code: &str) -> (u16, String) {
    crate::common::put_json_with_token(
        app,
        &format!("/api/derived_parameters/{id}"),
        &serde_json::json!({ "code": code }),
        token,
    )
    .await
}

async fn catalog_code(db: &DatabaseConnection, parameter_id: Uuid) -> String {
    db.query_one_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT code FROM parameters WHERE id = $1",
        [parameter_id.into()],
    ))
    .await
    .expect("the catalog reads")
    .expect("the calculation minted a parameter")
    .try_get::<String>("", "code")
    .expect("code")
}

fn output_of(created: &serde_json::Value) -> Uuid {
    Uuid::parse_str(
        created["output_parameter_id"]
            .as_str()
            .unwrap_or_else(|| panic!("the calculation publishes a parameter: {created}")),
    )
    .expect("a uuid")
}

#[tokio::test]
#[serial]
async fn an_unpublished_code_is_renamed_in_the_catalog_too() {
    let f = crate::common::seeded_app().await;
    let created = define(&f.db, &f.app, &f.token, "suva").await;
    let (id, output) = (created["id"].as_str().expect("id"), output_of(&created));
    assert_eq!(catalog_code(&f.db, output).await, "suva");

    let (status, body) = rename(&f.app, &f.token, id, "suva254").await;
    assert!((200..300).contains(&status), "rename ({status}): {body}");
    assert_eq!(
        catalog_code(&f.db, output).await,
        "suva254",
        "the code is the CSV header and the public identifier, so the catalog carries the rename"
    );
}

#[tokio::test]
#[serial]
async fn a_code_with_readings_under_it_is_not_renamed() {
    let f = crate::common::seeded_app().await;
    let created = define(&f.db, &f.app, &f.token, "suva_stored").await;
    let (id, output) = (created["id"].as_str().expect("id"), output_of(&created));
    f.db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, \
         replicate_index, measurement_type) \
         SELECT d.id, $1, $2, '2024-06-01T09:00:00Z', 3.4, 0, 'derived' \
           FROM data_streams d LIMIT 1",
        [
            Uuid::parse_str(crate::common::SITE1_ID).unwrap().into(),
            output.into(),
        ],
    ))
    .await
    .expect("a stored derived reading");

    let (status, body) = rename(&f.app, &f.token, id, "suva254").await;
    assert_eq!(status, 400, "the rename is refused: {body}");
    assert!(body.contains('1'), "the refusal names the count: {body}");
    assert_eq!(catalog_code(&f.db, output).await, "suva_stored");
}

#[tokio::test]
#[serial]
async fn a_code_a_project_publishes_is_not_renamed() {
    let f = crate::common::seeded_app().await;
    let created = define(&f.db, &f.app, &f.token, "suva_public").await;
    let (id, output) = (created["id"].as_str().expect("id"), output_of(&created));
    f.db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type, is_public) \
         VALUES (gen_random_uuid(), $1, $2, 'SUVA', 'derived', true)",
        [
            Uuid::parse_str(crate::common::SITE1_ID).unwrap().into(),
            output.into(),
        ],
    ))
    .await
    .expect("a public slot");

    let (status, body) = rename(&f.app, &f.token, id, "suva254").await;
    assert_eq!(status, 400, "the rename is refused: {body}");
    assert!(
        body.contains("Test River Project"),
        "the refusal names the project publishing it: {body}"
    );
    assert_eq!(catalog_code(&f.db, output).await, "suva_public");
}

#[tokio::test]
#[serial]
async fn a_first_save_does_not_adopt_a_parameter_no_calculation_produces() {
    let f = crate::common::seeded_app().await;
    let calculation = crate::common::seed_formula_calculation(&f.db, "turbidity_clash_set").await;
    let body = serde_json::json!({
        "code": "Turbidity",
        "name": "Not turbidity",
        "units": "L/mg/m",
        "formula": "Dissolved_O2 * 2",
        "tool_script_id": calculation,
    });
    let (status, text) =
        crate::common::post_json_with_token(&f.app, "/api/derived_parameters", &body, &f.token)
            .await;
    assert_eq!(status, 409, "the measurement is not taken over: {text}");
    assert!(
        text.contains("Turbidity"),
        "the refusal names the parameter holding the code: {text}"
    );
}

async fn count(db: &DatabaseConnection, sql: &str, id: Uuid) -> i64 {
    db.query_one_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        [id.into()],
    ))
    .await
    .expect("the count reads")
    .expect("one row")
    .try_get::<i64>("", "n")
    .expect("n")
}

async fn stored_output(db: &DatabaseConnection, definition_id: Uuid) -> Option<Uuid> {
    db.query_one_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT output_parameter_id FROM calculation_formulas WHERE id = $1",
        [definition_id.into()],
    ))
    .await
    .expect("the formula reads")
    .expect("the formula")
    .try_get::<Option<Uuid>>("", "output_parameter_id")
    .expect("output_parameter_id")
}

async fn set_step(app: &axum::Router, token: &str, id: &str, step: bool) -> (u16, String) {
    crate::common::put_json_with_token(
        app,
        &format!("/api/derived_parameters/{id}"),
        &serde_json::json!({ "intermediate": step }),
        token,
    )
    .await
}

/// Scenario: an output with readings stored under it is ticked as a step, then ticked back.
///
/// Expected behaviour: publication stops and starts again, and the identity survives it. The same
/// catalog parameter comes back, with every reading and every version it had.
#[tokio::test]
#[serial]
async fn an_output_ticked_as_a_step_and_back_keeps_its_parameter_readings_and_versions() {
    let f = crate::common::seeded_app().await;
    let created = define(&f.db, &f.app, &f.token, "m291_round").await;
    let id = created["id"].as_str().expect("id").to_string();
    let definition = Uuid::parse_str(&id).expect("a uuid");
    let output = output_of(&created);

    let (status, sp) = crate::common::post_json_with_token(
        &f.app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID, "parameter_id": output.to_string(),
            "name": "m291_round", "sensor_type": "derived", "entry_mode": "tool",
        }),
        &f.token,
    )
    .await;
    assert!((200..300).contains(&status), "assign ({status}): {sp}");
    assert!(
        crate::common::e2e::wait_for_jobs_by_trigger(&f.db, "derived_assignment", 30).await,
        "the assignment backfills the history it covers"
    );

    let readings = count(
        &f.db,
        "SELECT count(*) AS n FROM readings WHERE parameter_id = $1",
        output,
    )
    .await;
    assert!(readings > 0, "the output has a history to keep");

    // A version to lose: the formula was authored one at a time, which mints none, and an
    // assertion over an empty list would hold whatever either tick did to it.
    crate::common::exec(
        &f.db,
        &format!(
            "INSERT INTO tool_script_versions \
                 (tool_script_id, version_no, script, manifest, content_hash) \
             SELECT d.tool_script_id, 1, '', '{{}}'::jsonb, 'm291_round' \
               FROM calculation_formulas d WHERE d.id = '{definition}'"
        ),
    )
    .await;
    let versions_before = versions(&f.db, definition).await;
    assert_eq!(versions_before, "1", "the calculation holds one version");

    let (status, body) = set_step(&f.app, &f.token, &id, true).await;
    assert!(
        (200..300).contains(&status),
        "tick as a step ({status}): {body}"
    );
    assert_eq!(
        stored_output(&f.db, definition).await,
        None,
        "a step publishes nothing, so the link is given up"
    );

    let (status, body) = set_step(&f.app, &f.token, &id, false).await;
    assert!(
        (200..300).contains(&status),
        "tick back as an output ({status}): {body}"
    );
    assert_eq!(
        stored_output(&f.db, definition).await,
        Some(output),
        "re-enabling publication recovers the same parameter, not a second one"
    );
    assert_eq!(
        count(
            &f.db,
            "SELECT count(*) AS n FROM readings WHERE parameter_id = $1",
            output
        )
        .await,
        readings,
        "the readings stored under it are untouched by either tick"
    );
    assert_eq!(
        versions(&f.db, definition).await,
        versions_before,
        "neither tick mints a version or drops one"
    );
    assert_eq!(
        crate::common::e2e::scalar(
            &f.db,
            "SELECT count(*)::text FROM parameters WHERE LOWER(code) = LOWER('m291_round')"
        )
        .await,
        "1",
        "the code names one catalog row throughout, not a second one minted on the way back"
    );
}
