//! `POST /tool_scripts/{id}/formulas/draft_run`: an unsaved formula set run at a visit, in place
//! of the calculation's stored formulas, storing nothing.
//!
//! Run: cargo test --test tools formula_draft_run -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::keycloak as kc;
use crate::common::{GLOBAL_PARAM_DO_ID, SITE1_ID};

const CALCULATION: &str = "draft_chain";
const GROUP_ID: &str = "00000000-0000-4000-c000-000000000203";
const AT: &str = "2025-07-03T09:00:00Z";

/// A formula calculation over a `Peak` family, with no formulas saved: what the draft run is for.
async fn seed_calculation(db: &DatabaseConnection) -> (String, String) {
    for sql in [
        format!("UPDATE tool_scripts SET active_version_id = NULL WHERE name = '{CALCULATION}'"),
        format!("DELETE FROM tool_scripts WHERE name = '{CALCULATION}'"),
        format!(
            "INSERT INTO parameter_groups (id, code, label, ordinal) \
             VALUES ('{GROUP_ID}', 'draft_chain', 'Draft chain', 1)"
        ),
    ] {
        crate::common::exec(db, &sql).await;
    }
    let peak_id = uuid::Uuid::new_v4().to_string();
    for sql in [
        format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
             VALUES ('{peak_id}', 'Peak', 'Peak', 'ppb', 'measurement')"
        ),
        format!(
            "INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal) \
             VALUES (gen_random_uuid(), '{GROUP_ID}', '{peak_id}', 1)"
        ),
        format!(
            "INSERT INTO tool_scripts (name, label, engine, created_by) \
             VALUES ('{CALCULATION}', 'Draft chain', 'formula', 'test')"
        ),
    ] {
        crate::common::exec(db, &sql).await;
    }
    let script_id = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT id FROM tool_scripts WHERE name = '{CALCULATION}'"),
        ))
        .await
        .expect("query")
        .expect("the calculation")
        .try_get::<uuid::Uuid>("", "id")
        .expect("id")
        .to_string();
    (script_id, peak_id)
}

async fn setup() -> (DatabaseConnection, axum::Router, String, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::member_jwt("scriptadmin", "scriptadmin", "riverdata-admin").await;
    (db, app, token, admin)
}

async fn draft_run(
    app: &axum::Router,
    script_id: &str,
    payload: &serde_json::Value,
    admin: &str,
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        app,
        &format!("/api/tool_scripts/{script_id}/formulas/draft_run"),
        payload,
        admin,
    )
    .await
}

async fn count(db: &DatabaseConnection, table: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT count(*) AS n FROM {table}"),
    ))
    .await
    .expect("query")
    .expect("a row")
    .try_get("", "n")
    .expect("n")
}

/// Scenario: the operator has typed two formulas and saved neither, and the visit holds a DO
/// reading. Expected behaviour: the run evaluates the set as typed, reads the visit for the
/// scalar it names, reports the stage the set cannot yet reach with its reason, and writes
/// nothing.
#[tokio::test]
#[serial]
async fn an_unsaved_formula_set_runs_at_a_visit_and_stores_nothing() {
    let (db, app, token, admin) = setup().await;
    let (script_id, _peak_id) = seed_calculation(&db).await;
    let (status, body) = crate::common::post_checked_grab(
        &app,
        &json!({
            "site_id": SITE1_ID,
            "readings": [{ "parameter_id": GLOBAL_PARAM_DO_ID, "value": 8.0, "time": AT }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let before = (
        count(&db, "calculation_formulas").await,
        count(&db, "tool_script_versions").await,
        count(&db, "tool_runs").await,
        count(&db, "readings").await,
    );

    let (status, body) = draft_run(
        &app,
        &script_id,
        &json!({
            "formulas": [
                { "code": "S1", "formula": "Peak * 2", "ordinal": 1, "per_replicate": "Peak" },
                { "code": "S2", "formula": "S1 + Dissolved_O2", "ordinal": 2 },
                { "code": "Twice_DO", "formula": "Dissolved_O2 * 2", "ordinal": 3,
                  "intermediate": true },
            ],
            "inputs": { "Peak": [1.0, null, 3.0], "site_id": SITE1_ID, "collected_at": AT },
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ran"], json!(true), "{body}");
    assert_eq!(body["results"]["S1"], json!([2.0, null, 6.0]), "{body}");
    assert_eq!(
        body["results"]["Twice_DO"],
        json!(16.0),
        "the visit's DO is read: {body}"
    );
    assert!(
        body["skipped"]
            .as_array()
            .expect("skipped")
            .iter()
            .any(|s| s["output"] == json!("S2")),
        "S2 reads a mean no reading holds yet, so it is skipped with a reason: {body}"
    );
    assert!(
        body["event_inputs"]
            .as_array()
            .expect("event inputs")
            .iter()
            .any(|e| e["param"] == json!("Dissolved_O2") && e["value"] == json!(8.0)),
        "{body}"
    );
    let outputs: Vec<&str> = body["manifest"]["outputs"]
        .as_array()
        .expect("outputs")
        .iter()
        .filter_map(|o| o["key"].as_str())
        .collect();
    assert_eq!(
        outputs,
        ["S1", "S2"],
        "an intermediate is no output: {body}"
    );
    assert_eq!(
        body["manifest"]["params"][0]["kind"],
        json!("replicates"),
        "{body}"
    );

    let after = (
        count(&db, "calculation_formulas").await,
        count(&db, "tool_script_versions").await,
        count(&db, "tool_runs").await,
        count(&db, "readings").await,
    );
    assert_eq!(before, after, "a draft run writes nothing");
}

#[tokio::test]
#[serial]
async fn a_formula_naming_an_unknown_variable_is_refused_by_code() {
    let (db, app, _token, admin) = setup().await;
    let (script_id, _) = seed_calculation(&db).await;
    let (status, body) = draft_run(
        &app,
        &script_id,
        &json!({
            "formulas": [{ "code": "S1", "formula": "Nothing * 2", "ordinal": 1 }],
            "inputs": { "site_id": SITE1_ID, "collected_at": AT },
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let message = body["error"].as_str().unwrap_or_default().to_string() + &body.to_string();
    assert!(
        message.contains("S1") && message.contains("Nothing"),
        "{body}"
    );
}

#[tokio::test]
#[serial]
async fn a_script_calculation_takes_no_formula_draft() {
    let (db, app, _token, admin) = setup().await;
    let (script_id, _) = seed_calculation(&db).await;
    crate::common::exec(
        &db,
        &format!("UPDATE tool_scripts SET engine = 'script' WHERE id = '{script_id}'"),
    )
    .await;
    let (status, body) = draft_run(
        &app,
        &script_id,
        &json!({ "formulas": [{ "code": "S1", "formula": "1", "ordinal": 1 }] }),
        &admin,
    )
    .await;
    assert_eq!(status, 400, "{body}");
}

#[tokio::test]
#[serial]
async fn no_api_token_reaches_the_formula_draft_run() {
    let (db, app, token, _admin) = setup().await;
    let (script_id, _) = seed_calculation(&db).await;
    let (status, _) = draft_run(
        &app,
        &script_id,
        &json!({ "formulas": [{ "code": "S1", "formula": "1", "ordinal": 1 }] }),
        &token,
    )
    .await;
    assert!(
        status == 401 || status == 403,
        "an admin-only route: {status}"
    );
}
