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
    let run_id = result["run_id"]
        .as_str()
        .expect("the run was stored")
        .to_string();

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

// --- What a run consumed (Q215) ---

const VISIT: &str = "2025-06-15T10:00:00Z";

/// A set reading two visit parameters and a catalog constant, so the run consumes a reading, a
/// constant and its own steps.
async fn save_consuming_set(app: &axum::Router, token: &str, script_id: &str) {
    let (status, text) = crate::common::save_formula_set(
        app,
        token,
        script_id,
        json!([
            { "code": "half_temp", "units": "C", "formula": "DO_Temperature * 0.5", "ordinal": 1, "intermediate": true },
            { "code": "trace_ratio_out", "units": "ratio", "formula": "half_temp / Dissolved_O2 * gas_const_r_atm", "ordinal": 2 }
        ]),
    )
    .await;
    assert!((200..300).contains(&status), "save ({status}): {text}");
}

async fn consumed_of(db: &sea_orm::DatabaseConnection, run_id: &str) -> serde_json::Value {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT context->'consumed' AS consumed FROM tool_runs WHERE id = '{run_id}'"),
    ))
    .await
    .expect("query")
    .expect("the run")
    .try_get::<serde_json::Value>("", "consumed")
    .expect("consumed")
}

fn entry<'a>(consumed: &'a serde_json::Value, variable: &str) -> &'a serde_json::Value {
    consumed
        .as_array()
        .expect("a list")
        .iter()
        .find(|c| c["variable"] == variable)
        .unwrap_or_else(|| panic!("{variable} was consumed: {consumed}"))
}

/// Scenario: a calculation runs at a visit, reading two stored readings and a constant.
///
/// Expected behaviour: the stored run names, per input, the rows it read and the revision each
/// stood at: a reading nobody has touched at its arrival state, a constant and each step at the
/// revision the audit trail holds, and after the reading is corrected, the next run at the
/// corrected revision.
#[tokio::test]
#[serial]
async fn a_stored_run_names_the_revision_of_every_input_it_read() {
    use river_db::common::bulk_write;
    use river_db::routes::private::readings::models::{Kind, Origin};
    use river_db::routes::private::readings::service::{self as decisions, Decision, DecisionKey};

    let (db, app, token) = setup().await;
    let script_id = seed_calculation(&db, "00000000-0000-4000-c000-000000000122").await;
    save_consuming_set(&app, &token, &script_id).await;
    let temp = crate::common::sensor_lifecycle::create_paired_stream(
        &db,
        "consumed-temp",
        crate::common::PARAM_S1_TEMP_ID,
    )
    .await;
    let oxygen = crate::common::sensor_lifecycle::create_paired_stream(
        &db,
        "consumed-do",
        crate::common::PARAM_S1_DO_ID,
    )
    .await;
    for (stream, parameter, value) in [
        (temp, crate::common::GLOBAL_PARAM_TEMP_ID, 8.0),
        (oxygen, crate::common::GLOBAL_PARAM_DO_ID, 2.0),
    ] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, \
                 replicate_index, measurement_type) \
                 VALUES ('{stream}', '{}', '{parameter}', '{VISIT}', {value}, 0, 'spot')",
                crate::common::SITE1_ID
            ),
        )
        .await;
    }
    // The cleanup truncates the audit trail and keeps the seeded constants, so the constant has
    // no revision here until something writes it; on a deployment the migration backfilled one.
    // The trigger records nothing for an unchanged row, so the edit differs every run.
    crate::common::exec(
        &db,
        &format!(
            "UPDATE constants SET description = 'gas constant {}' WHERE name = 'gas_const_r_atm'",
            uuid::Uuid::new_v4()
        ),
    )
    .await;
    let at_visit = json!({ "site_id": crate::common::SITE1_ID, "collected_at": VISIT });

    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tools/{CALCULATION}/calculate"),
        &at_visit,
        &token,
    )
    .await;
    assert_eq!(status, 200, "calculate ({status}): {text}");
    let result: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let consumed = consumed_of(&db, result["run_id"].as_str().expect("run id")).await;

    let reading = entry(&consumed, "DO_Temperature");
    assert_eq!(reading["kind"], "reading", "{consumed}");
    assert_eq!(reading["members"][0]["stream_id"], json!(temp.to_string()));
    assert_eq!(reading["members"][0]["value"], json!(8.0));
    assert!(
        reading["members"][0]["revision"].is_null(),
        "a reading nobody has touched is at its arrival state: {consumed}"
    );
    let constant = entry(&consumed, "gas_const_r_atm");
    assert_eq!(constant["kind"], "constant");
    assert!(
        constant["subject"]
            .as_str()
            .is_some_and(|s| s.starts_with("constant:")),
        "{consumed}"
    );
    assert!(
        constant["revision"].as_i64().is_some(),
        "the edit is the constant's revision: {consumed}"
    );
    for code in ["half_temp", "trace_ratio_out"] {
        let step = entry(&consumed, code);
        assert_eq!(step["kind"], "step");
        assert!(
            step["subject"]
                .as_str()
                .is_some_and(|s| s.starts_with("calculation_formula:")),
            "{consumed}"
        );
        assert!(step["revision"].as_i64().is_some(), "{consumed}");
    }

    // The temperature is corrected, and the next run reads the corrected revision.
    let correction = Decision {
        key: DecisionKey {
            stream_id: temp,
            time: chrono::DateTime::parse_from_rfc3339(VISIT)
                .unwrap()
                .with_timezone(&chrono::Utc),
            replicate_index: Some(0),
        },
        kind: Kind::ValueCorrection,
        new: json!({ "raw_value": 9.0 }),
        actor: "tester".to_string(),
        reason: Some("typo".to_string()),
        origin: Origin::Manual,
        set_id: None,
    };
    bulk_write::guarded(&db, async |txn| decisions::record(txn, &correction).await)
        .await
        .expect("corrected");
    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tools/{CALCULATION}/calculate"),
        &at_visit,
        &token,
    )
    .await;
    assert_eq!(status, 200, "calculate ({status}): {text}");
    let result: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let consumed = consumed_of(&db, result["run_id"].as_str().expect("run id")).await;
    let reading = entry(&consumed, "DO_Temperature");
    assert_eq!(reading["members"][0]["value"], json!(9.0), "{consumed}");
    assert!(
        reading["members"][0]["revision"].as_i64().is_some(),
        "the correction is the reading's revision: {consumed}"
    );
}

/// Scenario: a reader arrives at the calculation page from a value computed at a visit, and the
/// page has to draw the recorded result rather than a fresh one.
///
/// Expected behaviour: the trace carries what the tables are drawn from: the values the run
/// produced, the outputs it skipped, the curves it applied and the manifest of the version it
/// pinned. Nothing here re-resolves anything from the store.
#[tokio::test]
#[serial]
async fn a_stored_run_carries_what_the_page_draws_it_from() {
    let (db, app, token) = setup().await;
    let script_id = seed_calculation(&db, "00000000-0000-4000-c000-000000000124").await;
    save_set(&app, &token, &script_id, 0.5).await;

    let result = calculate(&app, &token).await;
    let run_id = result["run_id"].as_str().expect("the run was stored");

    let (status, text) = trace_of(&app, &token, run_id).await;
    assert_eq!(status, 200, "trace ({status}): {text}");
    let replayed: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    // 8.0 * 0.5 / 2.0
    assert_eq!(
        replayed["results"]["trace_ratio_out"],
        json!(2.0),
        "the stored value, not a recomputation: {text}"
    );
    let outputs = replayed["manifest"]["outputs"]
        .as_array()
        .unwrap_or_else(|| panic!("the pinned manifest's outputs: {text}"));
    assert!(
        outputs.iter().any(|o| o["key"] == "trace_ratio_out"),
        "the manifest names the output the tables draw: {text}"
    );
    assert!(
        replayed["skipped"].is_array() && replayed["curves"].is_array(),
        "both are lists even when the run had none: {text}"
    );
}

/// Scenario: the runs at one visit are listed beside the calculation.
///
/// Expected behaviour: a run is found at its visit by column, so the list is one filtered read of
/// `/tool_runs`. A run stored before the columns existed is found there too, because the
/// migration backfilled it from the context blob it always carried.
#[tokio::test]
#[serial]
async fn the_runs_at_a_visit_are_found_by_site_and_instant() {
    let (db, app, token) = setup().await;
    let script_id = seed_calculation(&db, "00000000-0000-4000-c000-000000000125").await;
    save_set(&app, &token, &script_id, 0.5).await;

    let collected_at = "2025-01-15T00:00:00Z";
    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tools/{CALCULATION}/calculate"),
        &json!({
            "site_id": crate::common::SITE1_ID,
            "collected_at": collected_at,
            "DO_Temperature": 8.0,
            "Dissolved_O2": 2.0,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "calculate at a visit ({status}): {text}");
    let at_visit: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let run_id = at_visit["run_id"].as_str().expect("the run was stored");

    // A run whose columns were never written, as every row was before the migration.
    crate::common::exec(
        &db,
        &format!("UPDATE tool_runs SET site_id = NULL, collected_at = NULL WHERE id = '{run_id}'"),
    )
    .await;
    crate::common::exec(
        &db,
        "UPDATE tool_runs SET site_id = (context ->> 'site_id')::uuid, \
                              collected_at = (context ->> 'collected_at')::timestamptz \
          WHERE site_id IS NULL AND context ->> 'site_id' IS NOT NULL",
    )
    .await;

    let uri = format!(
        "/api/tool_runs?site_id={}&collected_at={collected_at}&tool_name={CALCULATION}",
        crate::common::SITE1_ID
    );
    let (status, listed) = crate::common::get_json_with_token(&app, &uri, &token).await;
    assert_eq!(status, 200, "list ({status}): {listed}");
    let rows = listed.as_array().expect("a list of runs");
    assert!(
        rows.iter().any(|r| r["id"].as_str() == Some(run_id)),
        "the run at this visit is in the list: {listed}"
    );
}
