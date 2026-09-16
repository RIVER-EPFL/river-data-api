//! `GET /tool_runs/{id}/trace`: a stored run replayed under the version it pinned, which is what
//! lets a computed value show its formula and its intermediates months after it was saved.

use sea_orm::{ConnectionTrait, Statement};
use serde_json::json;
use serial_test::serial;

const CALCULATION: &str = "trace_ratio";
/// The seeded `doc` version, which runs on the script engine.
const DOC_VERSION: &str = "abc3ee60-2bb7-474b-91e9-d76754dea651";

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

/// A group holding the two seeded parameters the formulas read, plus the calculation bound to it.
async fn seed_calculation(db: &sea_orm::DatabaseConnection, group_id: &str) -> String {
    for sql in [
        format!("UPDATE tool_scripts SET active_version_id = NULL WHERE name = '{CALCULATION}'"),
        format!("DELETE FROM tool_scripts WHERE name = '{CALCULATION}'"),
    ] {
        crate::common::exec(db, &sql).await;
    }
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO parameter_groups (id, code, label, ordinal) \
             VALUES ('{group_id}', 'thermal', 'Thermal', 1)"
        ),
    )
    .await;
    for (parameter, ordinal) in [
        (crate::common::GLOBAL_PARAM_TEMP_ID, 1),
        (crate::common::GLOBAL_PARAM_DO_ID, 2),
    ] {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal) \
                 VALUES (gen_random_uuid(), '{group_id}', '{parameter}', {ordinal})"
            ),
        )
        .await;
    }
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO tool_scripts (name, label, engine, created_by) \
             VALUES ('{CALCULATION}', 'Trace ratio', 'formula', 'test')"
        ),
    )
    .await;
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT id FROM tool_scripts WHERE name = '{CALCULATION}'"),
    ))
    .await
    .expect("query")
    .expect("the calculation")
    .try_get::<uuid::Uuid>("", "id")
    .expect("id")
    .to_string()
}

/// A step and the output that reads it, so the replayed trace has an intermediate to carry.
async fn save_set(app: &axum::Router, token: &str, script_id: &str, multiplier: f64) {
    let (status, text) = crate::common::save_formula_set(
        app,
        token,
        script_id,
        json!([
            { "code": "half_temp", "units": "C", "formula": format!("DO_Temperature * {multiplier}"), "ordinal": 1, "intermediate": true },
            { "code": "trace_ratio_out", "units": "ratio", "formula": "half_temp / Dissolved_O2", "ordinal": 2 }
        ]),
    )
    .await;
    assert!((200..300).contains(&status), "save ({status}): {text}");
}

async fn calculate(app: &axum::Router, token: &str) -> serde_json::Value {
    let (status, text) = crate::common::post_json_with_token(
        app,
        &format!("/api/tools/{CALCULATION}/calculate"),
        &json!({ "DO_Temperature": 8.0, "Dissolved_O2": 2.0 }),
        token,
    )
    .await;
    assert_eq!(status, 200, "calculate ({status}): {text}");
    serde_json::from_str(&text).expect("JSON")
}

async fn trace_of(app: &axum::Router, token: &str, run_id: &str) -> (u16, String) {
    crate::common::get_with_token(app, &format!("/api/tool_runs/{run_id}/trace"), token).await
}

/// Scenario: a value was computed, and later somebody asks where the number came from.
///
/// Expected behaviour: the stored run replays into the trace it returned when it ran, formula
/// text, intermediates and the bindings each formula read.
#[tokio::test]
#[serial]
async fn a_stored_run_replays_into_the_trace_it_returned() {
    let (db, app, token) = setup().await;
    let script_id = seed_calculation(&db, "00000000-0000-4000-c000-000000000121").await;
    save_set(&app, &token, &script_id, 0.5).await;

    let result = calculate(&app, &token).await;
    let run_id = result["run_id"].as_str().expect("the run was stored");
    assert!(
        !result["trace"].as_array().expect("a trace").is_empty(),
        "the run returned its own trace: {result}"
    );

    let (status, text) = trace_of(&app, &token, run_id).await;
    assert_eq!(status, 200, "trace ({status}): {text}");
    let replayed: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(
        replayed["trace"], result["trace"],
        "the replay is the run: {text}"
    );
    assert_eq!(replayed["tool"], CALCULATION);
    assert_eq!(
        replayed["trace"][0]["cells"][0]["bindings"]["DO_Temperature"],
        json!(8.0),
        "each formula names what it read: {text}"
    );
    assert_eq!(
        replayed["trace"][1]["cells"][0]["bindings"]["half_temp"],
        json!(4.0),
        "including the step it read from: {text}"
    );
}

/// Scenario: the calculation's formula is edited after a value was computed with the old one.
///
/// Expected behaviour: the old run still replays as it ran. The version is the formula set and a
/// run pins one, so a reader asking about last month's number is shown last month's arithmetic
/// rather than today's.
#[tokio::test]
#[serial]
async fn an_edited_formula_leaves_the_stored_run_replaying_as_it_ran() {
    let (db, app, token) = setup().await;
    let script_id = seed_calculation(&db, "00000000-0000-4000-c000-000000000122").await;
    save_set(&app, &token, &script_id, 0.5).await;
    let result = calculate(&app, &token).await;
    let run_id = result["run_id"].as_str().expect("the run was stored").to_string();

    save_set(&app, &token, &script_id, 3.0).await;
    let after = calculate(&app, &token).await;
    assert_ne!(
        after["results"]["trace_ratio_out"], result["results"]["trace_ratio_out"],
        "the edit changed what the calculation computes"
    );

    let (status, text) = trace_of(&app, &token, &run_id).await;
    assert_eq!(status, 200, "trace ({status}): {text}");
    let replayed: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(
        replayed["trace"], result["trace"],
        "the older run replays under the version it pinned: {text}"
    );
    assert_eq!(
        replayed["trace"][0]["formula"], "DO_Temperature * 0.5",
        "with the formula text as it stood: {text}"
    );
}

/// Scenario: the trace is asked for on a run of a script calculation.
///
/// Expected behaviour: refused, naming the engine. A script returns what it returns and records no
/// formulas, so there is nothing to replay and no number to show a reader step by step.
#[tokio::test]
#[serial]
async fn a_script_run_is_refused_naming_its_engine() {
    let (db, app, token) = setup().await;
    let run_id = uuid::Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO tool_runs (id, tool_name, tool_version, inputs, constants, curves, \
                 outputs, created_by, source) \
             VALUES ('{run_id}', 'doc', '{{\"script_version_id\": \"{DOC_VERSION}\"}}', \
                 '{{\"DOC\": [120.0]}}', '{{}}', '[]', '{{\"DOC_avg_ppb\": 120.0}}', 'test', \
                 'interactive')"
        ),
    )
    .await;

    let (status, text) = trace_of(&app, &token, &run_id.to_string()).await;
    assert_eq!(status, 409, "trace ({status}): {text}");
    assert!(
        text.contains("script"),
        "the refusal names the engine: {text}"
    );
}

/// Scenario: the trace is asked for on a run whose pinned version has been deleted.
///
/// Expected behaviour: refused rather than replayed under whatever is active now, which would
/// answer the reader's question with another calculation's arithmetic.
#[tokio::test]
#[serial]
async fn a_run_whose_version_is_gone_is_refused() {
    let (db, app, token) = setup().await;
    let run_id = uuid::Uuid::new_v4();
    let missing = uuid::Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO tool_runs (id, tool_name, tool_version, inputs, constants, curves, \
                 outputs, created_by, source) \
             VALUES ('{run_id}', '{CALCULATION}', '{{\"script_version_id\": \"{missing}\"}}', \
                 '{{}}', '{{}}', '[]', '{{}}', 'test', 'interactive')"
        ),
    )
    .await;

    let (status, text) = trace_of(&app, &token, &run_id.to_string()).await;
    assert_eq!(status, 409, "trace ({status}): {text}");
    assert!(
        text.contains("no longer stored"),
        "the refusal says the version is gone: {text}"
    );
}

/// Scenario: the trace is asked for on a run id nothing wrote.
#[tokio::test]
#[serial]
async fn an_unknown_run_is_not_found() {
    let (_db, app, token) = setup().await;
    let (status, _) = trace_of(&app, &token, &uuid::Uuid::new_v4().to_string()).await;
    assert_eq!(status, 404);
}
