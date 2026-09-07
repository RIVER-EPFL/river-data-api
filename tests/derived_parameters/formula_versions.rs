//! Scenario: a continuous derived value is computed, and later the formula that made it is edited.
//!
//! Expected behaviour: the stored value names the formula version it was made with, and an edit
//! mints a version rather than rewriting one, so the old text stays recoverable (Q89, M103, M113).

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
                "SELECT v.formula AS formula FROM readings r \
                   LEFT JOIN derived_parameter_definition_versions v ON v.id = r.derived_version_id \
                  WHERE r.parameter_id = $1 AND r.time = $2 LIMIT 1",
                [parameter_id.into(), time.into()],
            ))
            .await
            .ok()
            .flatten();
        if let Some(r) = row {
            return Some(r.try_get::<Option<String>>("", "formula").ok().flatten());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    None
}

async fn versions_of(db: &DatabaseConnection, code: &str) -> Vec<String> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT v.formula FROM derived_parameter_definition_versions v \
               JOIN derived_parameter_definitions d ON d.id = v.definition_id \
              WHERE d.code = $1 ORDER BY v.version_no",
            [code.into()],
        ))
        .await
        .expect("versions");
    rows.iter()
        .filter_map(|r| r.try_get::<String>("", "formula").ok())
        .collect()
}

#[tokio::test]
#[serial]
async fn a_computed_value_names_the_formula_it_was_made_with_and_an_edit_mints_a_version() {
    let (db, app, token) = setup().await;

    let code = format!("fver_{}", Uuid::new_v4().simple());
    let (status, def) = crate::common::post_json_parse_with_token(
        &app,
        "/api/derived_parameters",
        &serde_json::json!({
            "code": code,
            "name": "Formula version fixture",
            "units": "mg/L",
            "formula": "Dissolved_O2 * 0.032",
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create ({status}): {def}");
    let output = def["output_parameter_id"].as_str().expect("output").to_string();
    let definition_id = def["id"].as_str().expect("id").to_string();

    // Creating the definition mints version 1.
    assert_eq!(
        versions_of(&db, &code).await,
        vec!["Dissolved_O2 * 0.032".to_string()],
        "the definition's first text is its first version"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "parameter_id": output,
            "name": code,
            "sensor_type": "derived",
            "entry_mode": "tool",
            "display_units": "mg/L",
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "assign ({status}): {body}");

    let at: DateTime<Utc> = Utc::now() - Duration::hours(12);
    let at = at - Duration::nanoseconds(i64::from(at.timestamp_subsec_nanos()));
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &serde_json::json!({
            "readings": [{
                "site_id": crate::common::SITE1_ID,
                "parameter_id": crate::common::GLOBAL_PARAM_DO_ID,
                "time": at.to_rfc3339(),
                "raw_value": 250.0,
            }]
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "ingest ({status}): {body}");

    let parameter = Uuid::parse_str(&output).unwrap();
    let named = formula_of(&db, parameter, at)
        .await
        .expect("the derived value is computed");
    assert_eq!(
        named.as_deref(),
        Some("Dissolved_O2 * 0.032"),
        "the stored value names the text that produced it"
    );

    // An edit is a new calculation, not a correction of the old one: the first version stands.
    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/derived_parameters/{definition_id}"),
        &serde_json::json!({ "formula": "Dissolved_O2 * 0.064" }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "edit ({status}): {body}");
    assert_eq!(
        versions_of(&db, &code).await,
        vec![
            "Dissolved_O2 * 0.032".to_string(),
            "Dissolved_O2 * 0.064".to_string()
        ],
        "the edit mints a version and leaves the one the stored value names"
    );
}
