//! Scenario: a continuous derived value is computed, and later the set that made it is saved again
//! with a different formula.
//!
//! Expected behaviour: the stored value names the version of the calculation it was made with, and
//! a save mints a version rather than rewriting one, so the old text stays recoverable (Q89, M103,
//! M113). One version table answers for both arms (Q231): the value a visit produced and the value
//! a stream pass produced name rows of `tool_script_versions`.
//!
//! Run with: cargo test --test derived_parameters formula_versions

use chrono::{DateTime, Duration, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

const POLL_DEADLINE_SECS: u64 = 30;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

/// Save a calculation's whole set, the one act that mints a version (Q186).
async fn save_set(
    app: &axum::Router,
    token: &str,
    calculation: Uuid,
    code: &str,
    formula: &str,
) -> serde_json::Value {
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        &format!("/api/tool_scripts/{calculation}/formulas"),
        &serde_json::json!({
            "formulas": [{
                "code": code,
                "name": "Formula version fixture",
                "units": "mg/L",
                "formula": formula,
                "ordinal": 0,
            }]
        }),
        token,
    )
    .await;
    assert!((200..300).contains(&status), "save set ({status}): {body}");
    body
}

/// The output parameter the calculation's formula mints.
async fn output_parameter(db: &DatabaseConnection, code: &str) -> Uuid {
    db.query_one_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT output_parameter_id FROM calculation_formulas WHERE LOWER(code) = LOWER($1)",
        [code.into()],
    ))
    .await
    .expect("a query")
    .expect("the save minted the output")
    .try_get::<Uuid>("", "output_parameter_id")
    .expect("an output parameter")
}

async fn assign_slot(app: &axum::Router, token: &str, parameter_id: Uuid, code: &str) {
    let (status, body) = crate::common::post_json_with_token(
        app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "parameter_id": parameter_id,
            "name": code,
            "sensor_type": "derived",
            "entry_mode": "tool",
            "cadence": "high",
            "display_units": "mg/L",
        }),
        token,
    )
    .await;
    assert!((200..300).contains(&status), "assign ({status}): {body}");
}

async fn ingest_source(app: &axum::Router, token: &str, at: DateTime<Utc>, value: f64) {
    let (status, body) = crate::common::post_json_with_token(
        app,
        "/api/readings/batch",
        &serde_json::json!({
            "readings": [{
                "site_id": crate::common::SITE1_ID,
                "parameter_id": crate::common::GLOBAL_PARAM_DO_ID,
                "time": at.to_rfc3339(),
                "raw_value": value,
            }]
        }),
        token,
    )
    .await;
    assert!((200..300).contains(&status), "ingest ({status}): {body}");
}

/// The formula text the reading at this instant names, once one is stored. `None` while nothing is
/// there yet, so a caller can poll; `Some(None)` for a row naming no version.
async fn formula_of(
    db: &DatabaseConnection,
    parameter_id: Uuid,
    time: DateTime<Utc>,
) -> Option<Option<String>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(POLL_DEADLINE_SECS);
    while std::time::Instant::now() < deadline {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT v.script AS script FROM readings r \
                   LEFT JOIN tool_script_versions v ON v.id = r.derived_version_id \
                  WHERE r.parameter_id = $1 AND r.time = $2 LIMIT 1",
                [parameter_id.into(), time.into()],
            ))
            .await
            .ok()
            .flatten();
        if let Some(r) = row {
            let script = r.try_get::<Option<String>>("", "script").ok().flatten();
            return Some(script.and_then(|body| {
                serde_json::from_str::<Vec<serde_json::Value>>(&body)
                    .ok()?
                    .first()
                    .and_then(|f| f["formula"].as_str().map(str::to_string))
            }));
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    None
}

/// Every version a calculation holds, in order.
async fn versions_of(db: &DatabaseConnection, calculation: Uuid) -> Vec<String> {
    db.query_all_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT script FROM tool_script_versions WHERE tool_script_id = $1 ORDER BY version_no",
        [calculation.into()],
    ))
    .await
    .expect("versions")
    .iter()
    .filter_map(|r| r.try_get::<String>("", "script").ok())
    .filter_map(|body| {
        serde_json::from_str::<Vec<serde_json::Value>>(&body)
            .ok()?
            .first()
            .and_then(|f| f["formula"].as_str().map(str::to_string))
    })
    .collect()
}

#[tokio::test]
#[serial]
async fn a_computed_value_names_the_formula_it_was_made_with_and_an_edit_mints_a_version() {
    let (db, app, token) = setup().await;

    let code = format!("fver_{}", Uuid::new_v4().simple());
    let calculation = crate::common::seed_formula_calculation(&db, &format!("{code}_set")).await;
    save_set(&app, &token, calculation, &code, "Dissolved_O2 * 0.032").await;

    // Saving the set mints version 1.
    assert_eq!(
        versions_of(&db, calculation).await,
        vec!["Dissolved_O2 * 0.032".to_string()],
        "the set's first text is its first version"
    );

    let parameter = output_parameter(&db, &code).await;
    assign_slot(&app, &token, parameter, &code).await;

    let at: DateTime<Utc> = Utc::now() - Duration::hours(12);
    let at = at - Duration::nanoseconds(i64::from(at.timestamp_subsec_nanos()));
    ingest_source(&app, &token, at, 250.0).await;

    let named = formula_of(&db, parameter, at)
        .await
        .expect("the derived value is computed");
    assert_eq!(
        named.as_deref(),
        Some("Dissolved_O2 * 0.032"),
        "the stored value names the text that produced it"
    );

    // A second save is a new version, not a correction of the old one: the first stands, and the
    // value that names it still reads as made by it.
    save_set(&app, &token, calculation, &code, "Dissolved_O2 * 0.064").await;
    assert_eq!(
        versions_of(&db, calculation).await,
        vec![
            "Dissolved_O2 * 0.032".to_string(),
            "Dissolved_O2 * 0.064".to_string()
        ],
        "the save mints a version and leaves the one the stored value names"
    );
    assert_eq!(
        formula_of(&db, parameter, at).await.flatten().as_deref(),
        Some("Dissolved_O2 * 0.032"),
        "the stored value stays on the version that made it"
    );
}

/// Scenario: the record of a value a formula calculation computed is opened in the inspector.
///
/// Expected behaviour: it resolves the calculation that made it, the same way a value produced by
/// a script resolves its run (C94). A row stored before versioning names no version, and the
/// record says the formula is not recoverable rather than naming today's.
#[tokio::test]
#[serial]
async fn a_formula_value_resolves_the_calculation_that_produced_it() {
    let (db, app, token) = setup().await;

    let code = format!("fprov_{}", Uuid::new_v4().simple());
    let calculation = crate::common::seed_formula_calculation(&db, &format!("{code}_set")).await;
    save_set(&app, &token, calculation, &code, "Dissolved_O2 * 0.032").await;

    let parameter = output_parameter(&db, &code).await;
    assign_slot(&app, &token, parameter, &code).await;

    let at: DateTime<Utc> = Utc::now() - Duration::hours(11);
    let at = at - Duration::nanoseconds(i64::from(at.timestamp_subsec_nanos()));
    ingest_source(&app, &token, at, 250.0).await;
    formula_of(&db, parameter, at)
        .await
        .expect("the derived value is computed");

    let uri = format!(
        "/api/readings/provenance?site_id={}&parameter_id={}&time={}",
        crate::common::SITE1_ID,
        parameter,
        at.to_rfc3339().replace('+', "%2B")
    );
    let (status, body) = crate::common::get_json_with_token(&app, &uri, &token).await;
    assert_eq!(status, 200, "{body}");
    let calc = &body["records"][0]["calculation"];
    assert_eq!(
        calc["code"], code,
        "the record names the calculation: {body}"
    );
    assert_eq!(calc["formula"], "Dissolved_O2 * 0.032");
    assert_eq!(calc["version_no"], 1);
    assert_eq!(calc["active_version_no"], 1);
    assert!(calc["content_hash"].is_string());
    assert_eq!(
        calc["tool_script_id"],
        calculation.to_string(),
        "the record opens the calculation's page: {body}"
    );

    // A value stored before versioning names none, and the record says so rather than naming
    // today's formula.
    db.execute_unprepared(&format!(
        "UPDATE readings SET derived_version_id = NULL WHERE parameter_id = '{parameter}'"
    ))
    .await
    .expect("unstamp");
    let (status, body) = crate::common::get_json_with_token(&app, &uri, &token).await;
    assert_eq!(status, 200, "{body}");
    let calc = &body["records"][0]["calculation"];
    assert_eq!(calc["code"], code);
    assert!(
        calc["formula"].is_null() && calc["version_no"].is_null(),
        "an unversioned value names no formula: {body}"
    );
    assert_eq!(calc["active_version_no"], 1);
}
