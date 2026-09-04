//! A calculation with the formula engine: its formulas are `derived_parameter_definitions` rows,
//! its versions are minted from them, and it runs through the same calculate route a script
//! calculation does.
//!
//! The evaluation itself is unit-tested in `routes::private::tools::formula`; what only real SQL
//! can show is the version a formula edit mints, the manifest the formulas present, and the
//! group check refusing a calculation that writes outside its group.

use sea_orm::{ConnectionTrait, Statement};
use serde_json::json;
use serial_test::serial;

const CALCULATION: &str = "temp_ratio";

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
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
    for (parameter, role, ordinal) in [
        (crate::common::GLOBAL_PARAM_TEMP_ID, "measured", 1),
        (crate::common::GLOBAL_PARAM_DO_ID, "measured", 2),
    ] {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO parameter_group_members (id, group_id, parameter_id, role, ordinal) \
                 VALUES (gen_random_uuid(), '{group_id}', '{parameter}', '{role}', {ordinal})"
            ),
        )
        .await;
    }
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO tool_scripts (name, label, engine, parameter_group_id, created_by) \
             VALUES ('{CALCULATION}', 'Temperature ratio', 'formula', '{group_id}', 'test')"
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

/// The output parameter and its `output` membership exist before the formula does: a calculation
/// writes only what its group declares it writes, and the catalog row is a manager's act.
async fn declare_output(db: &sea_orm::DatabaseConnection, group_id: &str, code: &str) -> String {
    let parameter_id = uuid::Uuid::new_v4().to_string();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
             VALUES ('{parameter_id}', '{code}', '{code}', 'ratio', 'measurement')"
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO parameter_group_members (id, group_id, parameter_id, role, ordinal) \
             VALUES (gen_random_uuid(), '{group_id}', '{parameter_id}', 'output', 9)"
        ),
    )
    .await;
    parameter_id
}

async fn add_formula(
    app: &axum::Router,
    token: &str,
    script_id: &str,
    code: &str,
    formula: &str,
    ordinal: i32,
) -> (u16, String) {
    crate::common::post_json_with_token(
        app,
        "/api/derived_parameters",
        &json!({
            "code": code,
            "name": code,
            "units": "ratio",
            "formula": formula,
            "tool_script_id": script_id,
            "ordinal": ordinal,
        }),
        token,
    )
    .await
}

#[tokio::test]
#[serial]
async fn a_formula_edit_mints_and_activates_a_version() {
    let group_id = "00000000-0000-4000-c000-000000000101";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;
    declare_output(&db, group_id, "temp_ratio_out").await;
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
    assert!((200..300).contains(&status), "create ({status}): {text}");

    let (version_no, body) = active_version(&db).await.expect("a version was minted");
    assert_eq!(version_no, 1);
    assert!(
        body.contains("DO_Temperature / Dissolved_O2"),
        "the version body is the formula set: {body}"
    );

    let created: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let definition_id = created["id"].as_str().unwrap();
    let (status, text) = crate::common::put_json_with_token(
        &app,
        &format!("/api/derived_parameters/{definition_id}"),
        &json!({
            "code": "temp_ratio_out",
            "name": "temp_ratio_out",
            "units": "ratio",
            "formula": "Dissolved_O2 / DO_Temperature",
            "tool_script_id": script_id,
            "ordinal": 1,
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "update ({status}): {text}");

    let (version_no, body) = active_version(&db).await.expect("a second version");
    assert_eq!(version_no, 2, "the edit minted a version");
    assert!(
        body.contains("Dissolved_O2 / DO_Temperature"),
        "the active version is the edited formula set: {body}"
    );
}

#[tokio::test]
#[serial]
async fn an_unchanged_formula_set_mints_nothing() {
    let group_id = "00000000-0000-4000-c000-000000000102";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    let script_id = calculation_id(&db).await;
    declare_output(&db, group_id, "temp_ratio_out").await;

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
    let created: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let definition_id = created["id"].as_str().unwrap();

    let (status, text) = crate::common::put_json_with_token(
        &app,
        &format!("/api/derived_parameters/{definition_id}"),
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
    assert!((200..300).contains(&status), "update ({status}): {text}");
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
    declare_output(&db, group_id, "temp_ratio_out").await;
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

#[tokio::test]
#[serial]
async fn a_calculation_may_not_read_outside_its_group() {
    let group_id = "00000000-0000-4000-c000-000000000104";
    let (db, app, token) = setup().await;
    seed_calculation(&db, group_id).await;
    // Conductivity is in the catalog and in no group, so the calculation may not read it.
    let script_id = calculation_id(&db).await;
    declare_output(&db, group_id, "temp_ratio_out").await;
    let (status, text) = add_formula(
        &app,
        &token,
        &script_id,
        "temp_ratio_out",
        "DO_Temperature / Conductivity",
        1,
    )
    .await;
    assert_eq!(status, 400, "the create should be refused: {text}");
    assert!(
        text.contains("measured or entry_only member"),
        "the refusal says why: {text}"
    );
    assert!(
        active_version(&db).await.is_none(),
        "nothing was minted for a refused formula set"
    );
}
