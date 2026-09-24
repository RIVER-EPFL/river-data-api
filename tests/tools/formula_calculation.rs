//! A calculation with the formula engine: its formulas are `calculation_formulas` rows,
//! its versions are minted from them, and it runs through the same calculate route a script
//! calculation does.
//!
//! The evaluation itself is unit-tested in `routes::private::tools::service`; what only real SQL
//! can show is the version a formula edit mints, the manifest the formulas present, and the
//! group check refusing a calculation that writes outside its group.

use sea_orm::{ConnectionTrait, Statement};
use serde_json::json;
use serial_test::serial;

const CALCULATION: &str = "temp_ratio";

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

/// A group holding the two seeded parameters the formulas read, plus the calculation bound to it.
/// `tool_scripts` is authored through Administrator-only routes, so the row is written directly:
/// what this suite is about is what happens once a formula calculation exists.
async fn seed_calculation(db: &sea_orm::DatabaseConnection, group_id: &str) {
    // `cleanup_test_db` runs before this, and it preserves tool scripts as reference data, so a
    // fixture calculation removes its own the way every other tool fixture does.
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
             VALUES ('{CALCULATION}', 'Temperature ratio', 'formula', 'test')"
        ),
    )
    .await;
}

async fn calculation_id(db: &sea_orm::DatabaseConnection) -> String {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT id FROM tool_scripts WHERE name = '{CALCULATION}'"),
        ))
        .await
        .expect("query")
        .expect("the calculation");
    row.try_get::<uuid::Uuid>("", "id").expect("id").to_string()
}

async fn active_version(db: &sea_orm::DatabaseConnection) -> Option<(i32, String)> {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT v.version_no, v.script FROM tool_scripts s \
                   JOIN tool_script_versions v ON v.id = s.active_version_id \
                  WHERE s.name = '{CALCULATION}'"
            ),
        ))
        .await
        .expect("query")?;
    Some((
        row.try_get("", "version_no").expect("version_no"),
        row.try_get("", "script").expect("script"),
    ))
}

/// The catalog parameter a saved formula minted for its code. A calculation mints its own output
/// (Q191), so this is read after the formula is added, never inserted before it.
async fn minted_output(db: &sea_orm::DatabaseConnection, code: &str) -> String {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT id FROM parameters WHERE lower(code) = lower('{code}')"),
    ))
    .await
    .expect("the catalog reads")
    .expect("the formula minted its output")
    .try_get::<uuid::Uuid>("", "id")
    .expect("id")
    .to_string()
}

/// Author the calculation as one formula, through the save route: a formula row written through
/// CRUD mints no version, and the version is what the calculation runs.
async fn add_formula(
    app: &axum::Router,
    token: &str,
    script_id: &str,
    code: &str,
    formula: &str,
    ordinal: i32,
) -> (u16, String) {
    crate::common::save_formula_set(
        app,
        token,
        script_id,
        json!([{ "code": code, "name": code, "units": "ratio", "formula": formula, "ordinal": ordinal }]),
    )
    .await
}

/// Scenario: an author saves a calculation's formula set, then saves an edit of it.
///
/// Expected behaviour: each save is one version, activated, carrying the set as saved. The save is
/// the act that mints; the rows it writes on the way mint nothing of their own.
#[tokio::test]
#[serial]
async fn a_set_save_mints_and_activates_a_version() {
    let group_id = "00000000-0000-4000-c000-000000000101";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;
    assert!(
        active_version(&db).await.is_none(),
        "a calculation with no formulas has no version"
    );

    let (status, text) = add_formula(
        &app,
        &token,
        &script_id,
        "temp_ratio_out",
        "DO_Temperature / Dissolved_O2",
        1,
    )
    .await;
    assert!((200..300).contains(&status), "save ({status}): {text}");

    let (version_no, body) = active_version(&db).await.expect("a version was minted");
    assert_eq!(version_no, 1);
    assert!(
        body.contains("DO_Temperature / Dissolved_O2"),
        "the version body is the formula set: {body}"
    );

    let formula = formula_id(&db, "temp_ratio_out").await;
    let (status, text) = crate::common::save_formula_set(
        &app,
        &token,
        &script_id,
        json!([{
            "id": formula,
            "code": "temp_ratio_out",
            "units": "ratio",
            "formula": "Dissolved_O2 / DO_Temperature",
            "ordinal": 1,
        }]),
    )
    .await;
    assert!((200..300).contains(&status), "edit ({status}): {text}");

    let (version_no, body) = active_version(&db).await.expect("a second version");
    assert_eq!(version_no, 2, "the edit minted a version");
    assert!(
        body.contains("Dissolved_O2 / DO_Temperature"),
        "the active version is the edited formula set: {body}"
    );
}

/// Scenario: a formula row is written through its CRUD route, which is what the entity list and
/// the older forms do.
///
/// Expected behaviour: the row is written and nothing is minted. One save is one version (Q186),
/// so a version comes from the save route and from nowhere else; a row write that minted one made
/// the history a list of keystrokes, and re-minted every other formula calculation besides.
#[tokio::test]
#[serial]
async fn a_formula_row_written_through_crud_mints_no_version() {
    let group_id = "00000000-0000-4000-c000-000000000111";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;

    let (status, text) = crate::common::post_json_with_token(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "temp_ratio_out",
            "name": "temp_ratio_out",
            "units": "ratio",
            "formula": "DO_Temperature / Dissolved_O2",
            "tool_script_id": script_id,
            "ordinal": 1,
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create ({status}): {text}");
    assert_eq!(
        formula_codes(&db).await,
        ["temp_ratio_out"],
        "the row is written"
    );
    assert!(
        active_version(&db).await.is_none(),
        "and no version was minted by the row"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) FROM tool_script_activations a \
                   JOIN tool_scripts s ON s.id = a.tool_script_id WHERE s.name = '{CALCULATION}'"
            )
        )
        .await,
        0,
        "nor an activation"
    );
}

/// Scenario: the calculation's formula is written through CRUD, then the same set is saved.
///
/// Expected behaviour: the chain reads a calculation only once a save has pinned a version. Until
/// then the calculation is invisible to `calculations_fed_by`, so a write at a visit enqueues no
/// recompute and the output stays empty.
#[tokio::test]
#[serial]
async fn the_chain_reads_a_calculation_only_once_a_save_pins_a_version() {
    let group_id = "00000000-0000-4000-c000-000000000113";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;
    let temperature: uuid::Uuid = crate::common::GLOBAL_PARAM_TEMP_ID.parse().expect("uuid");

    let (status, text) = crate::common::post_json_with_token(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "temp_ratio_out",
            "name": "temp_ratio_out",
            "units": "ratio",
            "formula": "DO_Temperature / Dissolved_O2",
            "tool_script_id": script_id,
            "ordinal": 1,
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create ({status}): {text}");
    let fed = river_db::routes::private::tools::service::calculations_fed_by(&db, &[temperature])
        .await
        .expect("the graph reads");
    assert!(
        !fed.iter().any(|c| c.tool == CALCULATION),
        "an unpinned calculation feeds nothing: {:?}",
        fed.iter().map(|c| &c.tool).collect::<Vec<_>>()
    );

    let (status, text) = add_formula(
        &app,
        &token,
        &script_id,
        "temp_ratio_out",
        "DO_Temperature / Dissolved_O2",
        1,
    )
    .await;
    assert!((200..300).contains(&status), "save ({status}): {text}");
    let fed = river_db::routes::private::tools::service::calculations_fed_by(&db, &[temperature])
        .await
        .expect("the graph reads");
    let reading_it = fed
        .iter()
        .find(|c| c.tool == CALCULATION)
        .unwrap_or_else(|| {
            panic!(
                "the saved calculation reads the temperature: {:?}",
                fed.iter().map(|c| &c.tool).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        reading_it
            .outputs
            .iter()
            .map(|o| o.parameter_code.as_str())
            .collect::<Vec<_>>(),
        ["temp_ratio_out"],
        "and writes the output the save minted"
    );
}

/// Scenario: a three-formula calculation saved in one go, by a service token rather than a person.
///
/// Expected behaviour: one version and one activation, not one per row. The version's
/// `created_by` is the token's label, which resolves through `api_tokens.created_by` to the
/// administrator who minted it (Q196).
#[tokio::test]
#[serial]
async fn a_set_save_mints_one_version_and_one_activation() {
    let group_id = "00000000-0000-4000-c000-000000000112";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;

    let (status, text) = crate::common::save_formula_set(
        &app,
        &token,
        &script_id,
        json!([
            { "code": "set_a", "units": "ratio", "formula": "DO_Temperature / Dissolved_O2", "ordinal": 1 },
            { "code": "set_b", "units": "ratio", "formula": "DO_Temperature * 2", "ordinal": 2 },
            { "code": "set_c", "units": "ratio", "formula": "Dissolved_O2 + 1", "ordinal": 3 }
        ]),
    )
    .await;
    assert!((200..300).contains(&status), "save ({status}): {text}");
    assert_eq!(formula_codes(&db).await, ["set_a", "set_b", "set_c"]);
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) FROM tool_script_versions v JOIN tool_scripts s ON s.id = \
                 v.tool_script_id WHERE s.name = '{CALCULATION}'"
            )
        )
        .await,
        1,
        "three formulas, one version"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) FROM tool_script_activations a JOIN tool_scripts s ON s.id = \
                 a.tool_script_id WHERE s.name = '{CALCULATION}'"
            )
        )
        .await,
        1,
        "and one activation"
    );

    let created_by = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT v.created_by FROM tool_script_versions v \
                   JOIN tool_scripts s ON s.id = v.tool_script_id WHERE s.name = '{CALCULATION}'"
            ),
        ))
        .await
        .expect("query")
        .expect("the version")
        .try_get::<Option<String>>("", "created_by")
        .expect("created_by")
        .expect("the save names who made it");
    assert!(
        created_by.contains("token"),
        "the minting is attributed to the token that made it: {created_by}"
    );
}

/// Scenario: the same formula set is saved twice, which is what a form does when nothing was
/// changed before pressing save.
///
/// Expected behaviour: the second save mints nothing. The content hash is what a version is keyed
/// on, so an unchanged set is the version that is already active.
#[tokio::test]
#[serial]
async fn an_unchanged_formula_set_mints_nothing() {
    let group_id = "00000000-0000-4000-c000-000000000102";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;

    let (status, text) = add_formula(
        &app,
        &token,
        &script_id,
        "temp_ratio_out",
        "DO_Temperature / Dissolved_O2",
        1,
    )
    .await;
    assert!((200..300).contains(&status), "save ({status}): {text}");

    // The second save is the form's own set: the formula it already holds, unchanged.
    let (status, text) = crate::common::save_formula_set(
        &app,
        &token,
        &script_id,
        json!([{
            "id": formula_id(&db, "temp_ratio_out").await,
            "code": "temp_ratio_out",
            "units": "ratio",
            "formula": "DO_Temperature / Dissolved_O2",
            "ordinal": 1,
        }]),
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "second save ({status}): {text}"
    );

    let (version_no, _) = active_version(&db).await.expect("the first version");
    assert_eq!(version_no, 1, "the same formula set is the same version");
}

#[tokio::test]
#[serial]
async fn the_calculation_runs_its_formulas_without_the_runner() {
    let group_id = "00000000-0000-4000-c000-000000000103";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;
    let (status, text) = add_formula(
        &app,
        &token,
        &script_id,
        "temp_ratio_out",
        "DO_Temperature / Dissolved_O2",
        1,
    )
    .await;
    assert!((200..300).contains(&status), "create ({status}): {text}");

    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tools/{CALCULATION}/calculate"),
        &json!({ "DO_Temperature": 8.0, "Dissolved_O2": 2.0 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "calculate ({status}): {text}");
    let result: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let value = result["results"]["temp_ratio_out"]
        .as_f64()
        .unwrap_or_else(|| panic!("no result: {text}"));
    assert!((value - 4.0).abs() < 1e-12, "8 / 2: {text}");
}

/// Scenario: a form previews a calculation as values are typed (Q212).
///
/// Expected behaviour: the preview returns the calculation `calculate` returns, stores no run and
/// carries no `run_id`, so there is nothing a save can name.
#[tokio::test]
#[serial]
async fn a_preview_computes_without_storing_a_run() {
    let group_id = "00000000-0000-4000-c000-000000000113";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;
    let (status, text) = add_formula(
        &app,
        &token,
        &script_id,
        "temp_ratio_out",
        "DO_Temperature / Dissolved_O2",
        1,
    )
    .await;
    assert!((200..300).contains(&status), "create ({status}): {text}");
    let runs = "SELECT count(*) AS count FROM tool_runs";
    let before = count(&db, runs).await;

    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tools/{CALCULATION}/preview"),
        &json!({ "DO_Temperature": 8.0, "Dissolved_O2": 2.0 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "preview ({status}): {text}");
    let preview: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let value = preview["results"]["temp_ratio_out"]
        .as_f64()
        .unwrap_or_else(|| panic!("no result: {text}"));
    assert!((value - 4.0).abs() < 1e-12, "8 / 2: {text}");
    assert!(
        preview.get("run_id").is_none(),
        "a preview names no run: {text}"
    );
    assert!(
        preview["trace"].as_array().is_some_and(|t| !t.is_empty()),
        "the full calculation, trace included: {text}"
    );
    assert_eq!(count(&db, runs).await, before, "a preview stores no run");

    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tools/{CALCULATION}/calculate"),
        &json!({ "DO_Temperature": 8.0, "Dissolved_O2": 2.0 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "calculate ({status}): {text}");
    let stored: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(
        stored["results"], preview["results"],
        "the same calculation"
    );
    assert!(
        stored["run_id"].is_string(),
        "calculate still names its run: {text}"
    );
    assert_eq!(
        count(&db, runs).await,
        before + 1,
        "calculate stores one run"
    );
}

/// Scenario: the two-stage shape four of the portal's calculators have, a formula evaluating once
/// per replicate index of the family it reads.
///
/// Expected behaviour: the run's output is one value per index with the gaps in place, the shape
/// the save path stores as one reading per replicate index.
#[tokio::test]
#[serial]
async fn a_per_replicate_formula_produces_one_value_per_index() {
    let group_id = "00000000-0000-4000-c000-000000000105";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;
    let (status, text) = crate::common::save_formula_set(
        &app,
        &token,
        &script_id,
        json!([{
            "code": "temp_ratio_out",
            "units": "ratio",
            "formula": "DO_Temperature * 2",
            "ordinal": 1,
            "per_replicate": "DO_Temperature",
        }]),
    )
    .await;
    assert!((200..300).contains(&status), "save ({status}): {text}");

    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tools/{CALCULATION}/calculate"),
        &json!({ "DO_Temperature": [1.0, null, 3.0] }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "calculate ({status}): {text}");
    let result: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(
        result["results"]["temp_ratio_out"],
        json!([2.0, null, 6.0]),
        "one value per index, the unmeasured repeat still at index 1: {text}"
    );
}

/// Scenario: an author edits a formula, which is a change to what the calculation computes.
///
/// Expected behaviour: the edit enqueues the audit itself, so what it leaves disagreeing is found
/// without anybody thinking to press Audit, and queues the recompute of the values the replaced
/// version produced (Q256).
#[tokio::test]
#[serial]
async fn a_formula_edit_enqueues_its_own_audit() {
    let group_id = "00000000-0000-4000-c000-000000000106";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;

    let audits = |db: sea_orm::DatabaseConnection| async move {
        db.query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT count(*)::bigint AS n FROM reprocessing_jobs \
                  WHERE trigger_type = 'event_audit' \
                    AND params ->> 'calculation' = '{CALCULATION}'"
            ),
        ))
        .await
        .expect("query")
        .expect("a row")
        .try_get::<i64>("", "n")
        .expect("n")
    };

    let (status, text) = add_formula(
        &app,
        &token,
        &script_id,
        "temp_ratio_out",
        "DO_Temperature / Dissolved_O2",
        1,
    )
    .await;
    assert!((200..300).contains(&status), "save ({status}): {text}");
    let after_create = audits(db.clone()).await;
    assert!(
        after_create >= 1,
        "the first version is an activation and audits: {after_create}"
    );

    let (status, text) = crate::common::save_formula_set(
        &app,
        &token,
        &script_id,
        json!([{
            "id": formula_id(&db, "temp_ratio_out").await,
            "code": "temp_ratio_out",
            "units": "ratio",
            "formula": "Dissolved_O2 / DO_Temperature",
            "ordinal": 1,
        }]),
    )
    .await;
    assert!((200..300).contains(&status), "edit ({status}): {text}");
    assert!(
        audits(db.clone()).await >= after_create,
        "the edit audits too"
    );

    let rewrites = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*)::bigint AS n FROM reprocessing_jobs \
              WHERE trigger_type = 'event_recompute'"
                .to_string(),
        ))
        .await
        .expect("query")
        .expect("a row")
        .try_get::<i64>("", "n")
        .expect("n");
    assert_eq!(rewrites, 1, "the edit queues the recompute of what the first version produced");
}

/// Scenario: a calculation reading a catalog parameter that belongs to no group of its own, and
/// writing an output nobody declared on the group first.
///
/// Expected behaviour: both are taken. Q135 made the role a property of the calculation and the
/// group a way to list many parameters together, so nothing here is a boundary. This is the
/// inverse of the refusal that stood until then.
#[tokio::test]
#[serial]
async fn a_calculation_reads_any_catalog_parameter_and_declares_its_own_output() {
    let group_id = "00000000-0000-4000-c000-000000000104";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    // Conductivity is in the catalog and in no group; the output is declared by this formula
    // alone, with no member row written by hand first.
    let script_id = calculation_id(&db).await;
    let (status, text) = add_formula(
        &app,
        &token,
        &script_id,
        "temp_ratio_out",
        "DO_Temperature / Conductivity",
        1,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "the create is taken ({status}): {text}"
    );
    assert!(
        active_version(&db).await.is_some(),
        "and its version is minted, so the calculation runs"
    );
}

/// The harness owns the rows a formula calculation leaves behind: a formula is a
/// `calculation_formulas` row with `derived_parameter_sources` under it, and neither FK
/// cascades, so a cleanup that deletes the calculation first is refused and every later test in
/// the binary fails at setup rather than in its body.
#[tokio::test]
#[serial]
async fn cleanup_removes_a_formula_calculation_with_its_sources() {
    let group_id = "00000000-0000-4000-c000-000000000105";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;
    let (status, text) = add_formula(
        &app,
        &token,
        &script_id,
        "temp_ratio_out",
        "DO_Temperature / Dissolved_O2",
        1,
    )
    .await;
    assert!((200..300).contains(&status), "create ({status}): {text}");
    assert!(
        count(&db, "SELECT count(*) FROM derived_parameter_sources").await > 0,
        "the formula recorded its sources"
    );

    crate::common::cleanup_test_db(&db).await;

    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM tool_scripts WHERE created_by = 'test'"
        )
        .await,
        0,
        "the calculation is gone"
    );
    assert_eq!(
        count(&db, "SELECT count(*) FROM calculation_formulas").await,
        0,
        "its formulas went with it"
    );
    assert_eq!(
        count(&db, "SELECT count(*) FROM derived_parameter_sources").await,
        0,
        "and so did their sources"
    );
}

async fn count(db: &sea_orm::DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .expect("query")
    .expect("a row")
    .try_get::<i64>("", "count")
    .expect("count")
}

/// Scenario: a calculation computes a step and the formula after it reads that step, which is how
/// the portal's pressure guard reaches the corrections that use it.
///
/// Expected behaviour: the step's value reaches the formula after it. An intermediate stores
/// nothing and mints no parameter, so recording it as a source would send the run looking for a
/// reading of it and skip the formula that reads it.
#[tokio::test]
#[serial]
async fn a_formula_reads_the_step_before_it() {
    let group_id = "00000000-0000-4000-c000-000000000107";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;

    let (status, text) = crate::common::save_formula_set(
        &app,
        &token,
        &script_id,
        json!([
            { "code": "half_temp", "units": "ratio", "formula": "DO_Temperature / 2", "ordinal": 1, "intermediate": true },
            { "code": "temp_ratio_out", "units": "ratio", "formula": "half_temp + Dissolved_O2", "ordinal": 2 }
        ]),
    )
    .await;
    assert!((200..300).contains(&status), "save ({status}): {text}");

    let sources = crate::common::e2e::count(
        &db,
        "SELECT count(*) FROM derived_parameter_sources WHERE variable_name = 'half_temp'",
    )
    .await;
    assert_eq!(
        sources, 0,
        "a step is no source: it names no stored reading"
    );

    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tools/{CALCULATION}/calculate"),
        &json!({ "DO_Temperature": 8.0, "Dissolved_O2": 2.0 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "calculate ({status}): {text}");
    let result: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let value = result["results"]["temp_ratio_out"]
        .as_f64()
        .unwrap_or_else(|| panic!("no result: {text}"));
    assert!((value - 6.0).abs() < 1e-12, "8 / 2 + 2: {text}");
}

const VISIT_TIME: &str = "2025-07-02T08:00:00Z";

/// A visit holding the two inputs, plus whatever the output slot already serves.
async fn seed_visit(db: &sea_orm::DatabaseConnection, readings: &[(&str, f64)]) -> uuid::Uuid {
    let event_id = uuid::Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO collection_events (id, site_id, collected_at, source) \
             VALUES ('{event_id}', '{}', '{VISIT_TIME}', 'manual')",
            crate::common::SITE1_ID
        ),
    )
    .await;
    for (parameter_id, value) in readings {
        let stream_id = uuid::Uuid::new_v4();
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO data_streams (id, source_system, source_key, is_active) \
                 VALUES ('{stream_id}', 'grab_sample', '{stream_id}', true)"
            ),
        )
        .await;
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                     raw_value, measurement_type, collection_event_id) \
                 VALUES ('{stream_id}', '{}', '{parameter_id}', '{VISIT_TIME}', 0, {value}, \
                     'spot', '{event_id}')",
                crate::common::SITE1_ID
            ),
        )
        .await;
    }
    event_id
}

/// The slot the calculation writes, as the site declares it: a lab column filled at a visit.
async fn declare_slot(db: &sea_orm::DatabaseConnection, parameter_id: &str) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, \
                 decimal_places, is_active, cadence) \
             VALUES (gen_random_uuid(), '{}', '{parameter_id}', 'temp_ratio_out', \
                 3, true, 'low')",
            crate::common::SITE1_ID
        ),
    )
    .await;
}

/// What the output slot serves at the visit, and whether it was retracted.
async fn served(db: &sea_orm::DatabaseConnection, parameter_id: &str) -> Option<(f64, bool)> {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT raw_value, withdrawn_at IS NOT NULL AS withdrawn FROM readings \
                  WHERE site_id = '{}' AND parameter_id = '{parameter_id}' \
                    AND time = '{VISIT_TIME}'",
                crate::common::SITE1_ID
            ),
        ))
        .await
        .expect("query")?;
    Some((
        row.try_get::<Option<f64>>("", "raw_value")
            .expect("raw_value")
            .expect("a value"),
        row.try_get("", "withdrawn").expect("withdrawn"),
    ))
}

/// Scenario: a new visit is typed into the grid's spare row, so there is no visit to preview at yet.
///
/// Expected behaviour: the preview at the row's site and instant computes the output the chain
/// would write, and opens no visit.
#[tokio::test]
#[serial]
async fn a_preview_at_a_site_and_instant_computes_without_opening_a_visit() {
    let group_id = "00000000-0000-4000-c000-000000000114";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;
    let (status, text) = add_formula(
        &app,
        &token,
        &script_id,
        "temp_ratio_out",
        "DO_Temperature / Dissolved_O2",
        1,
    )
    .await;
    assert!((200..300).contains(&status), "create ({status}): {text}");
    let output_id = minted_output(&db, "temp_ratio_out").await;
    declare_slot(&db, &output_id).await;
    let visits = "SELECT count(*) AS count FROM collection_events";
    let before = count(&db, visits).await;

    let (status, text) = crate::common::post_json_with_token(
        &app,
        "/api/collection_events/preview",
        &json!({
            "site_id": crate::common::SITE1_ID,
            "collected_at": VISIT_TIME,
            "staged": [
                { "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID, "replicate_index": 0, "value": 8.0 },
                { "parameter_id": crate::common::GLOBAL_PARAM_DO_ID, "replicate_index": 0, "value": 2.0 }
            ]
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "preview ({status}): {text}");
    let preview: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let output = preview["outputs"]
        .as_array()
        .and_then(|o| o.iter().find(|v| v["parameter_id"] == output_id.as_str()))
        .unwrap_or_else(|| panic!("the output is previewed: {text}"));
    assert!(
        (output["value"].as_f64().expect("a value") - 4.0).abs() < 1e-12,
        "8 / 2: {text}"
    );
    assert_eq!(count(&db, visits).await, before, "a preview opens no visit");
}

/// Scenario: a visit where the divisor was entered as 0, so the formula computes Inf.
///
/// Expected behaviour: the output is refused (Q172). The value the slot already serves stands,
/// nothing is withdrawn, and a `skipped_output` finding names the arithmetic. An Inf reaching
/// `serde_json` becomes null, which is the portal's NA and would retract the stored value.
#[tokio::test]
#[serial]
async fn a_zero_divisor_refuses_the_output_and_leaves_the_stored_value() {
    let group_id = "00000000-0000-4000-c000-000000000107";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;
    let (status, text) = add_formula(
        &app,
        &token,
        &script_id,
        "temp_ratio_out",
        "DO_Temperature / Dissolved_O2",
        1,
    )
    .await;
    assert!((200..300).contains(&status), "create ({status}): {text}");
    let output_id = minted_output(&db, "temp_ratio_out").await;
    declare_slot(&db, &output_id).await;

    let event_id = seed_visit(
        &db,
        &[
            (crate::common::GLOBAL_PARAM_TEMP_ID, 8.0),
            (crate::common::GLOBAL_PARAM_DO_ID, 0.0),
            (output_id.as_str(), 4.0),
        ],
    )
    .await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let outcome =
        river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
            .await
            .expect("a refused output is not a failed recompute");
    assert_eq!(outcome.readings_withdrawn, 0, "nothing was retracted");
    assert_eq!(
        served(&db, &output_id).await,
        Some((4.0, false)),
        "the stored value stands and stays served"
    );

    let hold = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT kind, status, expected->>'reason' AS reason FROM replicate_audit_holds \
                  WHERE site_id = '{}' AND parameter_id = '{output_id}' \
                    AND group_time = '{VISIT_TIME}'",
                crate::common::SITE1_ID
            ),
        ))
        .await
        .expect("query")
        .expect("the refusal is on the record");
    assert_eq!(
        hold.try_get::<String>("", "kind").expect("kind"),
        "skipped_output"
    );
    assert_eq!(
        hold.try_get::<String>("", "status").expect("status"),
        "pending"
    );
    let reason = hold
        .try_get::<Option<String>>("", "reason")
        .expect("reason")
        .expect("a reason");
    assert!(
        reason.contains("not a finite number"),
        "the finding says what the arithmetic did: {reason}"
    );
    assert!(
        outcome.findings_raised >= 1,
        "the run reports the finding: {}",
        outcome.findings_raised
    );
}

/// Scenario: a visit where the formula computes NA, so the recompute withdraws the output the slot
/// served and saves nothing in its place.
///
/// Expected behaviour: the withdrawal is announced on the event bus for the output's slot, since
/// that announcement is what drops the site's cached series; the save path, which announces on
/// its own, is not reached.
#[tokio::test]
#[serial]
async fn a_recompute_that_only_withdraws_announces_the_slot_it_moved() {
    let group_id = "00000000-0000-4000-c000-000000000108";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;
    let (status, text) = add_formula(
        &app,
        &token,
        &script_id,
        "temp_ratio_out",
        "if(lt(DO_Temperature, 0), na, DO_Temperature / Dissolved_O2)",
        1,
    )
    .await;
    assert!((200..300).contains(&status), "create ({status}): {text}");
    let output_id = minted_output(&db, "temp_ratio_out").await;
    declare_slot(&db, &output_id).await;

    let event_id = seed_visit(
        &db,
        &[
            (crate::common::GLOBAL_PARAM_TEMP_ID, -1.0),
            (crate::common::GLOBAL_PARAM_DO_ID, 2.0),
            (output_id.as_str(), 4.0),
        ],
    )
    .await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());
    let mut bus = state.events.subscribe();

    let outcome =
        river_db::routes::private::tools::flows::recompute_event(&state, event_id, "test")
            .await
            .expect("the recompute runs");
    assert_eq!(outcome.readings_withdrawn, 1, "the NA blanks the column");
    assert_eq!(
        outcome.readings_written, 0,
        "an NA writes no value of its own"
    );
    assert_eq!(
        served(&db, &output_id).await,
        Some((4.0, true)),
        "the stored value is withdrawn"
    );

    let output = uuid::Uuid::parse_str(&output_id).expect("uuid");
    let site = uuid::Uuid::parse_str(crate::common::SITE1_ID).expect("uuid");
    let mut announced = Vec::new();
    while let Ok(event) = bus.try_recv() {
        if let river_db::common::AppEvent::DataIngested {
            site_id,
            parameter_id,
            ..
        } = event
        {
            announced.push((site_id, parameter_id));
        }
    }
    assert!(
        announced.contains(&(Some(site), Some(output))),
        "the withdrawal names the output's slot on the bus: {announced:?}"
    );
}

/// Scenario: an author edits a three-formula calculation and saves it.
///
/// Expected behaviour: the set the save sent is the set the calculation holds, and the save
/// activates a version of it (Q186). Authoring is Administrator-only, so the save carries a JWT.
#[tokio::test]
#[serial]
async fn a_set_save_writes_the_whole_set_and_activates_a_version() {
    if !crate::common::profile::Service::Keycloak
        .require("a_set_save_writes_the_whole_set_and_activates_a_version")
        .await
    {
        return;
    }
    let group_id = "00000000-0000-4000-c000-000000000109";
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;
    // A calculation mints its own outputs (Q191): nothing is declared for set_a..c first.
    let app = crate::common::keycloak::build_test_app_with_keycloak(db.clone()).await;
    let admin =
        crate::common::keycloak::member_jwt("setadmin", "setadmin", "riverdata-admin").await;

    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tool_scripts/{script_id}/formulas"),
        &json!({
            "formulas": [
                { "code": "set_a", "units": "ratio", "formula": "DO_Temperature / Dissolved_O2", "ordinal": 1 },
                { "code": "set_b", "units": "ratio", "formula": "DO_Temperature * 2", "ordinal": 2 },
                { "code": "set_c", "units": "ratio", "formula": "Dissolved_O2 + 1", "ordinal": 3 }
            ]
        }),
        &admin,
    )
    .await;
    assert!((200..300).contains(&status), "save ({status}): {text}");
    let saved: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(saved["created"], 3, "three formulas were created: {text}");
    assert_eq!(saved["deleted"], 0, "and nothing was deleted: {text}");
    assert_eq!(formula_codes(&db).await, ["set_a", "set_b", "set_c"]);
    let (_version_no, body) = active_version(&db).await.expect("a version was minted");
    for formula in [
        "DO_Temperature / Dissolved_O2",
        "DO_Temperature * 2",
        "Dissolved_O2 + 1",
    ] {
        assert!(
            body.contains(formula),
            "the version carries {formula}: {body}"
        );
    }

    // The set is the request: the save drops what it leaves out and updates what it names.
    let keep = formula_id(&db, "set_a").await;
    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tool_scripts/{script_id}/formulas"),
        &json!({
            "formulas": [
                { "id": keep, "code": "set_a", "units": "ratio", "formula": "DO_Temperature / 2", "ordinal": 1 }
            ]
        }),
        &admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "second save ({status}): {text}"
    );
    let saved: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(saved["updated"], 1, "the formula it named: {text}");
    assert_eq!(saved["deleted"], 2, "the two it left out: {text}");
    assert_eq!(formula_codes(&db).await, ["set_a"]);
    let (_version_no, body) = active_version(&db).await.expect("a version");
    assert!(
        body.contains("DO_Temperature / 2"),
        "the active version is the saved set: {body}"
    );

    // A code is unique across calculations, so a second calculation reusing one is told which.
    let (status, created) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tool_scripts",
        &json!({ "name": "set_other", "label": "Set other", "engine": "formula" }),
        &admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "second calculation: {created}"
    );
    let other_id = created["id"].as_str().expect("id");
    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tool_scripts/{other_id}/formulas"),
        &json!({
            "formulas": [
                { "code": "set_a", "units": "ratio", "formula": "DO_Temperature * 3", "ordinal": 1 }
            ]
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 400, "a taken code is refused: {text}");
    assert!(
        text.contains(&format!("set_a is a formula of {CALCULATION}")),
        "the refusal names the code and the calculation holding it: {text}"
    );
}

/// The calculation's formula codes, in evaluation order.
async fn formula_codes(db: &sea_orm::DatabaseConnection) -> Vec<String> {
    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT f.code FROM calculation_formulas f \
                   JOIN tool_scripts s ON s.id = f.tool_script_id \
                  WHERE s.name = '{CALCULATION}' ORDER BY f.ordinal"
            ),
        ))
        .await
        .expect("query");
    rows.into_iter()
        .map(|r| r.try_get::<String>("", "code").expect("code"))
        .collect()
}

async fn formula_id(db: &sea_orm::DatabaseConnection, code: &str) -> String {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT id FROM calculation_formulas WHERE code = '{code}'"),
    ))
    .await
    .expect("query")
    .expect("the formula")
    .try_get::<uuid::Uuid>("", "id")
    .expect("id")
    .to_string()
}

/// Scenario: an author ticks a published output of a calculation as a step, which is how
/// publication is disabled.
///
/// Expected behaviour: the save names what it stopped publishing, so the page can say it. Nothing
/// is deleted: the response carries the parameter, the readings that stay under it and the
/// formulas that still read it.
#[tokio::test]
#[serial]
async fn a_set_save_names_the_output_it_stopped_publishing() {
    let group_id = "00000000-0000-4000-c000-000000000102";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;

    let (status, text) = crate::common::save_formula_set(
        &app,
        &token,
        &script_id,
        json!([
            { "code": "temp_step", "name": "Temp step", "units": "ratio",
              "formula": "DO_Temperature / Dissolved_O2", "ordinal": 1 },
            { "code": "temp_reader", "name": "Temp reader", "units": "ratio",
              "formula": "temp_step * 2", "ordinal": 2 },
        ]),
    )
    .await;
    assert!((200..300).contains(&status), "save ({status}): {text}");
    let parameter = minted_output(&db, "temp_step").await;

    let step = formula_id(&db, "temp_step").await;
    let reader = formula_id(&db, "temp_reader").await;
    let (status, text) = crate::common::save_formula_set(
        &app,
        &token,
        &script_id,
        json!([
            { "id": step, "code": "temp_step", "units": "ratio",
              "formula": "DO_Temperature / Dissolved_O2", "ordinal": 1, "intermediate": true },
            { "id": reader, "code": "temp_reader", "units": "ratio",
              "formula": "temp_step * 2", "ordinal": 2 },
        ]),
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "tick as a step ({status}): {text}"
    );

    let saved: serde_json::Value = serde_json::from_str(&text).expect("the save response");
    let given_up = saved["given_up"].as_array().expect("given_up");
    assert_eq!(given_up.len(), 1, "one output stopped publishing: {saved}");
    assert_eq!(given_up[0]["code"], "temp_step");
    assert_eq!(given_up[0]["parameter_id"], parameter);
    assert_eq!(
        given_up[0]["readings_retained"], 0,
        "the count is of what stays under the parameter: {saved}"
    );

    let (status, text) = crate::common::save_formula_set(
        &app,
        &token,
        &script_id,
        json!([
            { "id": step, "code": "temp_step", "units": "ratio",
              "formula": "DO_Temperature / Dissolved_O2", "ordinal": 1 },
            { "id": reader, "code": "temp_reader", "units": "ratio",
              "formula": "temp_step * 2", "ordinal": 2 },
        ]),
    )
    .await;
    assert!((200..300).contains(&status), "tick back ({status}): {text}");
    let saved: serde_json::Value = serde_json::from_str(&text).expect("the save response");
    assert!(
        saved["given_up"].is_null() || saved["given_up"].as_array().expect("given_up").is_empty(),
        "a save that publishes again gives up nothing: {saved}"
    );
    assert_eq!(
        minted_output(&db, "temp_step").await,
        parameter,
        "ticking it back publishes the same catalog row, not a second one"
    );
}

/// Scenario: an author saves a correction, and the migration job it owes
/// cannot be queued.
///
/// Expected behaviour: the save and its migration are one transaction, so the save is refused and
/// the calculation stays on the version whose values nothing was queued to repair.
#[tokio::test]
#[serial]
async fn a_correcting_save_whose_migration_cannot_be_queued_saves_nothing() {
    let group_id = "00000000-0000-4000-c000-000000000130";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;
    let (status, text) = add_formula(
        &app,
        &token,
        &script_id,
        "queued_out",
        "DO_Temperature / Dissolved_O2",
        1,
    )
    .await;
    assert!((200..300).contains(&status), "save ({status}): {text}");
    let (was, _) = active_version(&db).await.expect("a version");

    crate::common::exec(
        &db,
        "CREATE FUNCTION refuse_version_migration() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.trigger_type = 'event_recompute' THEN \
         RAISE EXCEPTION 'the migration is refused'; END IF; RETURN NEW; END $$",
    )
    .await;
    crate::common::exec(
        &db,
        "CREATE TRIGGER refuse_version_migration BEFORE INSERT ON reprocessing_jobs \
         FOR EACH ROW EXECUTE FUNCTION refuse_version_migration()",
    )
    .await;
    let id = formula_id(&db, "queued_out").await;
    let (status, text) = crate::common::post_json_with_token(
        &app,
        &format!("/api/tool_scripts/{script_id}/formulas"),
        &json!({
            "formulas": [{ "id": id, "code": "queued_out", "units": "ratio",
                           "formula": "DO_Temperature * 2", "ordinal": 1 }],
        }),
        &token,
    )
    .await;
    crate::common::exec(
        &db,
        "DROP TRIGGER refuse_version_migration ON reprocessing_jobs",
    )
    .await;
    crate::common::exec(&db, "DROP FUNCTION refuse_version_migration()").await;

    assert!(
        !(200..300).contains(&status),
        "a save whose migration was not queued is refused ({status}): {text}"
    );
    let (now, body) = active_version(&db).await.expect("a version");
    assert_eq!(now, was, "the calculation stays on its version");
    assert!(
        !body.contains("DO_Temperature * 2"),
        "and the correction is not stored: {body}"
    );
}
