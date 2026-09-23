//! Parameter groups: the rules SQL holds, and the definition document the grid renders from.
//!
//! The move and delete decisions themselves are unit-tested in
//! `routes::private::parameter_groups::rules`; what only real SQL can show is the unique
//! membership, the history trigger firing on every writer, and the document the endpoint
//! assembles from a group and the catalog.

use sea_orm::{ConnectionTrait, Statement};
use serde_json::json;
use serial_test::serial;

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

async fn create_group(app: &axum::Router, token: &str, code: &str) -> serde_json::Value {
    let (status, text) = crate::common::post_json_with_token(
        app,
        "/api/parameter_groups",
        &json!({ "code": code, "label": code.to_uppercase(), "ordinal": 1 }),
        token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create group ({status}): {text}"
    );
    serde_json::from_str(&text).expect("group body is JSON")
}

async fn add_member(
    app: &axum::Router,
    token: &str,
    group_id: &str,
    parameter_id: &str,
    ordinal: i32,
) -> (u16, String) {
    crate::common::post_json_with_token(
        app,
        "/api/parameter_group_members",
        &json!({
            "group_id": group_id,
            "parameter_id": parameter_id,
            "ordinal": ordinal,
        }),
        token,
    )
    .await
}

async fn history_count(db: &sea_orm::DatabaseConnection, group_id: &str) -> i64 {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*) AS n FROM change_audit WHERE subject = 'parameter_group:' || $1",
            [group_id.into()],
        ))
        .await
        .expect("history query")
        .expect("one row");
    row.try_get("", "n").expect("count")
}

#[tokio::test]
#[serial]
async fn a_parameter_belongs_to_at_most_one_group() {
    let (_db, app, token) = setup().await;
    let dom = create_group(&app, &token, "dom").await;
    let ions = create_group(&app, &token, "ions").await;
    let dom_id = dom["id"].as_str().unwrap();
    let ions_id = ions["id"].as_str().unwrap();

    let (status, text) =
        add_member(&app, &token, dom_id, crate::common::GLOBAL_PARAM_TEMP_ID, 0).await;
    assert!((200..300).contains(&status), "first add ({status}): {text}");

    let (status, text) = add_member(
        &app,
        &token,
        ions_id,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        0,
    )
    .await;
    assert_eq!(status, 400, "second add should be refused: {text}");
    assert!(
        text.contains(dom_id),
        "the refusal names the group that holds it: {text}"
    );
}

#[tokio::test]
#[serial]
async fn a_group_with_members_is_not_deleted() {
    let (_db, app, token) = setup().await;
    let group = create_group(&app, &token, "dom").await;
    let group_id = group["id"].as_str().unwrap().to_string();
    let (status, text) = add_member(
        &app,
        &token,
        &group_id,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        0,
    )
    .await;
    assert!((200..300).contains(&status), "add ({status}): {text}");

    let (status, text) = crate::common::delete_with_token(
        &app,
        &format!("/api/parameter_groups/{group_id}"),
        &token,
    )
    .await;
    assert_eq!(status, 400, "delete should be refused: {text}");
    assert!(
        text.contains("1 members"),
        "the refusal counts them: {text}"
    );
}

#[tokio::test]
#[serial]
async fn every_change_appends_a_history_row() {
    let (db, app, token) = setup().await;
    let group = create_group(&app, &token, "dom").await;
    let group_id = group["id"].as_str().unwrap().to_string();
    assert_eq!(history_count(&db, &group_id).await, 1, "the create");

    let (status, text) = add_member(
        &app,
        &token,
        &group_id,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        0,
    )
    .await;
    assert!((200..300).contains(&status), "add ({status}): {text}");
    assert_eq!(history_count(&db, &group_id).await, 2, "the membership");

    let member: serde_json::Value = serde_json::from_str(&text).expect("member body is JSON");
    let member_id = member["id"].as_str().unwrap();
    let (status, text) = crate::common::put_json_with_token(
        &app,
        &format!("/api/parameter_group_members/{member_id}"),
        &json!({ "label": "Dissolved organic matter" }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "update ({status}): {text}");

    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT change, old_value->>'label' AS old_label, new_value->>'label' AS new_label \
               FROM change_audit \
              WHERE subject = 'parameter_group:' || $1 AND change = 'member_update'",
            [group_id.as_str().into()],
        ))
        .await
        .expect("history query")
        .expect("an update row");
    let old_label: Option<String> = row.try_get("", "old_label").unwrap();
    let new_label: Option<String> = row.try_get("", "new_label").unwrap();
    assert_eq!(old_label, None);
    assert_eq!(new_label.as_deref(), Some("Dissolved organic matter"));
}

#[tokio::test]
#[serial]
async fn the_definition_document_is_the_group_in_its_own_order() {
    let (db, app, token) = setup().await;
    let group = create_group(&app, &token, "dom").await;
    let group_id = group["id"].as_str().unwrap().to_string();

    add_member(
        &app,
        &token,
        &group_id,
        crate::common::GLOBAL_PARAM_DO_ID,
        2,
    )
    .await;
    add_member(
        &app,
        &token,
        &group_id,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        1,
    )
    .await;

    let (status, text) = crate::common::get_with_token(
        &app,
        &format!("/api/parameter_groups/{group_id}/definition"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "definition ({status}): {text}");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("definition is JSON");

    assert_eq!(doc["code"], "dom");
    let members = doc["members"].as_array().expect("members array");
    assert_eq!(members.len(), 2);
    assert_eq!(
        members[0]["parameter_id"],
        crate::common::GLOBAL_PARAM_TEMP_ID
    );
    // No calculation touches either yet, so neither is measured nor an output: the role is what
    // the calculations make of the parameter, never what the membership row was created with
    // (Q135).
    assert_eq!(members[0]["role"], "entry_only", "{text}");
    assert_eq!(
        members[1]["parameter_id"],
        crate::common::GLOBAL_PARAM_DO_ID
    );
    assert_eq!(members[1]["role"], "entry_only", "{text}");

    // One formula writing DO and reading Temperature moves both.
    let formula_id = uuid::Uuid::new_v4();
    let calculation = crate::common::seed_formula_calculation(&db, "do_from_temp_set").await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO calculation_formulas \
                 (id, code, name, formula, output_parameter_id, ordinal, tool_script_id) \
             VALUES ('{formula_id}', 'do_from_temp', 'DO from temperature', 'Temperature * 2', \
                     '{}', 0, '{calculation}')",
            crate::common::GLOBAL_PARAM_DO_ID
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO derived_parameter_sources \
                 (id, derived_definition_id, parameter_id, variable_name) \
             VALUES (gen_random_uuid(), '{formula_id}', '{}', 'Temperature')",
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;

    let (status, text) = crate::common::get_with_token(
        &app,
        &format!("/api/parameter_groups/{group_id}/definition"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "definition ({status}): {text}");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("definition is JSON");
    let members = doc["members"].as_array().expect("members array");
    assert_eq!(members[0]["role"], "measured", "read by a formula: {text}");
    assert_eq!(members[1]["role"], "output", "written by one: {text}");
    assert!(
        members[0]["label"].as_str().is_some_and(|l| !l.is_empty()),
        "the label falls back to the catalog name: {text}"
    );
}

#[tokio::test]
#[serial]
async fn an_unknown_group_has_no_definition() {
    let (_db, app, token) = setup().await;
    let (status, _) = crate::common::get_with_token(
        &app,
        "/api/parameter_groups/00000000-0000-4000-c000-0000000000ff/definition",
        &token,
    )
    .await;
    assert_eq!(status, 404);
}

#[tokio::test]
#[serial]
async fn the_document_carries_the_calculation_s_sections_without_reordering() {
    let (db, app, token) = setup().await;
    let group = create_group(&app, &token, "dom").await;
    let group_id = group["id"].as_str().unwrap().to_string();

    add_member(
        &app,
        &token,
        &group_id,
        crate::common::GLOBAL_PARAM_DO_ID,
        1,
    )
    .await;
    add_member(
        &app,
        &token,
        &group_id,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        2,
    )
    .await;

    // The manifest declares the thermal section first. The ordinals are the order.
    let manifest = json!({
        "label": "DOM",
        "params": [
            { "name": "t", "label": "T", "kind": "number",
              "parameter_code": "DO_Temperature", "section": "thermal" },
            { "name": "o", "label": "O", "kind": "number",
              "parameter_code": "Dissolved_O2", "section": "gases" }
        ],
        "outputs": [],
        "sections": [
            { "key": "thermal", "label": "Thermal" },
            { "key": "gases", "label": "Gases" }
        ]
    });
    let script_id = "00000000-0000-4000-c000-0000000002a1";
    let version_id = "00000000-0000-4000-c000-0000000002a2";
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO tool_scripts (id, name, label, engine, created_by) \
             VALUES ('{script_id}', 'dom_sections', 'DOM', 'script', 'test')"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO tool_script_versions \
                 (id, tool_script_id, version_no, script, manifest, content_hash) \
             VALUES ('{version_id}', '{script_id}', 1, 'tool <- function() list()', \
                     '{payload}'::jsonb, 'sections-fixture')",
            payload = manifest.to_string().replace('\'', "''")
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE tool_scripts SET active_version_id = '{version_id}' WHERE id = '{script_id}'"
        ),
    )
    .await;

    let (status, text) = crate::common::get_with_token(
        &app,
        &format!("/api/parameter_groups/{group_id}/definition"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "definition ({status}): {text}");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let members = doc["members"].as_array().expect("members");
    assert_eq!(
        members[0]["parameter_id"],
        crate::common::GLOBAL_PARAM_DO_ID
    );
    assert_eq!(members[0]["section"], "gases");
    assert_eq!(
        members[1]["parameter_id"],
        crate::common::GLOBAL_PARAM_TEMP_ID
    );
    assert_eq!(members[1]["section"], "thermal");
    assert_eq!(
        doc["sections"],
        serde_json::json!(["gases", "thermal"]),
        "sections follow the columns, not the manifest's own order: {text}"
    );
}

/// The parameter ids each group's definition document lists, in its own order.
async fn definition_members(app: &axum::Router, token: &str, group_id: &str) -> Vec<String> {
    let (status, text) = crate::common::get_with_token(
        app,
        &format!("/api/parameter_groups/{group_id}/definition"),
        token,
    )
    .await;
    assert_eq!(status, 200, "definition ({status}): {text}");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("definition is JSON");
    doc["members"]
        .as_array()
        .expect("members array")
        .iter()
        .map(|m| {
            m["parameter_id"]
                .as_str()
                .expect("parameter_id")
                .to_string()
        })
        .collect()
}

async fn member_id(db: &sea_orm::DatabaseConnection, group_id: &str, parameter_id: &str) -> String {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM parameter_group_members \
               WHERE group_id = $1::uuid AND parameter_id = $2::uuid",
            [group_id.into(), parameter_id.into()],
        ))
        .await
        .expect("member query")
        .expect("the member");
    row.try_get::<uuid::Uuid>("", "id").expect("id").to_string()
}

async fn move_member(app: &axum::Router, token: &str, id: &str, to_group: &str) -> (u16, String) {
    crate::common::put_json_with_token(
        app,
        &format!("/api/parameter_group_members/{id}"),
        &json!({ "group_id": to_group }),
        token,
    )
    .await
}

async fn reading_count(db: &sea_orm::DatabaseConnection) -> i64 {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*) AS n FROM readings".to_string(),
        ))
        .await
        .expect("readings query")
        .expect("one row");
    row.try_get("", "n").expect("count")
}

// Scenario: a category is reshaped, which is the operation Evan expects once the portal is the
// only place data is entered.
// Expected behaviour: the member moves, both definition documents say so, both groups record it,
// and not one reading is touched: a group says how a measurement is presented, never what it is.
#[tokio::test]
#[serial]
async fn a_measured_member_moves_and_the_readings_do_not() {
    let (db, app, token) = setup().await;
    let dom = create_group(&app, &token, "dom").await;
    let ions = create_group(&app, &token, "ions").await;
    let dom_id = dom["id"].as_str().unwrap().to_string();
    let ions_id = ions["id"].as_str().unwrap().to_string();

    add_member(
        &app,
        &token,
        &dom_id,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        1,
    )
    .await;
    let readings_before = reading_count(&db).await;
    let dom_history_before = history_count(&db, &dom_id).await;
    let ions_history_before = history_count(&db, &ions_id).await;

    let id = member_id(&db, &dom_id, crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let (status, text) = move_member(&app, &token, &id, &ions_id).await;
    assert!((200..300).contains(&status), "move ({status}): {text}");

    assert!(
        definition_members(&app, &token, &dom_id).await.is_empty(),
        "the member left dom"
    );
    assert_eq!(
        definition_members(&app, &token, &ions_id).await,
        vec![crate::common::GLOBAL_PARAM_TEMP_ID.to_string()],
        "and arrived in ions"
    );
    assert!(
        history_count(&db, &ions_id).await > ions_history_before,
        "the group it arrived in records the move"
    );
    assert!(
        history_count(&db, &dom_id).await >= dom_history_before,
        "the group it left is not rewritten backwards"
    );
    assert_eq!(
        reading_count(&db).await,
        readings_before,
        "a reshape moves no data"
    );
}

// Expected behaviour: a group is split by creating the second group and moving members across,
// and at no point does a parameter belong to two groups.
#[tokio::test]
#[serial]
async fn a_split_leaves_every_member_in_exactly_one_group() {
    let (db, app, token) = setup().await;
    let whole = create_group(&app, &token, "field_data").await;
    let split = create_group(&app, &token, "gauge").await;
    let whole_id = whole["id"].as_str().unwrap().to_string();
    let split_id = split["id"].as_str().unwrap().to_string();

    for (ordinal, parameter) in [
        crate::common::GLOBAL_PARAM_TEMP_ID,
        crate::common::GLOBAL_PARAM_DO_ID,
        crate::common::GLOBAL_PARAM_DEPTH_ID,
    ]
    .into_iter()
    .enumerate()
    {
        let (status, text) = add_member(
            &app,
            &token,
            &whole_id,
            parameter,
            i32::try_from(ordinal).expect("small"),
        )
        .await;
        assert!((200..300).contains(&status), "add ({status}): {text}");
    }

    let id = member_id(&db, &whole_id, crate::common::GLOBAL_PARAM_DEPTH_ID).await;
    let (status, text) = move_member(&app, &token, &id, &split_id).await;
    assert!((200..300).contains(&status), "move ({status}): {text}");

    let stayed = definition_members(&app, &token, &whole_id).await;
    let moved = definition_members(&app, &token, &split_id).await;
    assert_eq!(stayed.len(), 2, "two stayed: {stayed:?}");
    assert_eq!(
        moved,
        vec![crate::common::GLOBAL_PARAM_DEPTH_ID.to_string()],
        "one moved"
    );
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*) AS n FROM (SELECT parameter_id FROM parameter_group_members \
               GROUP BY parameter_id HAVING count(*) > 1) d"
                .to_string(),
        ))
        .await
        .expect("duplicate query")
        .expect("one row");
    assert_eq!(
        row.try_get::<i64>("", "n").expect("count"),
        0,
        "no parameter is in two groups after the split"
    );
}

// Scenario: someone moves an output out of the group whose calculation writes it.
// Expected behaviour: refused, naming the calculation, so the grid can never show a column no
// group produces or two groups claim.
#[tokio::test]
#[serial]
async fn an_output_member_cannot_leave_the_calculation_that_produces_it() {
    let (db, app, token) = setup().await;
    let dom = create_group(&app, &token, "dom").await;
    let ions = create_group(&app, &token, "ions").await;
    let dom_id = dom["id"].as_str().unwrap().to_string();
    let ions_id = ions["id"].as_str().unwrap().to_string();

    add_member(
        &app,
        &token,
        &dom_id,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        1,
    )
    .await;
    add_member(&app, &token, &dom_id, crate::common::GLOBAL_PARAM_DO_ID, 2).await;
    seed_group_calculation(&db, "suva").await;

    let id = member_id(&db, &dom_id, crate::common::GLOBAL_PARAM_DO_ID).await;
    let (status, text) = move_member(&app, &token, &id, &ions_id).await;
    assert_eq!(status, 400, "the move should be refused: {text}");
    assert!(
        text.contains("suva"),
        "the refusal names the calculation: {text}"
    );

    assert_eq!(
        definition_members(&app, &token, &dom_id).await.len(),
        2,
        "and the member stayed"
    );
}

/// A calculation reading the seeded temperature and producing dissolved oxygen. It names no group:
/// what makes it a calculation of a group is that a group holds those parameters (Q169).
/// `tool_scripts` is authored through Administrator-only routes, so the rows are written directly,
/// as the formula suite does.
async fn seed_group_calculation(db: &sea_orm::DatabaseConnection, name: &str) {
    let manifest = json!({
        "params": [{ "name": "temp", "kind": "number", "parameter_code": "DO_Temperature" }],
        "outputs": [{ "name": "do", "suggested_parameter_code": "Dissolved_O2" }],
    });
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO tool_scripts (id, name, label, engine, created_by) \
             VALUES ('00000000-0000-4000-d000-0000000000a1', '{name}', '{name}', 'formula', \
                     'test')"
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO tool_script_versions \
               (id, tool_script_id, version_no, script, entry_function, manifest, test_cases, \
                content_hash, created_by) \
             VALUES ('00000000-0000-4000-d000-0000000000a2', \
                     '00000000-0000-4000-d000-0000000000a1', 1, '[]', 'formula', \
                     '{manifest}'::jsonb, '[]'::jsonb, 'seed-{name}', 'test')"
        ),
    )
    .await;
    crate::common::exec(
        db,
        "UPDATE tool_scripts SET active_version_id = '00000000-0000-4000-d000-0000000000a2' \
           WHERE id = '00000000-0000-4000-d000-0000000000a1'",
    )
    .await;
}

/// Scenario: the plan recorded what the portal computed a column with, and the lab is about to
/// author that calculation by hand (Q149).
///
/// Expected behaviour: the definition the calculation page reads carries the portal function and
/// the columns it reads, so the author has the reference in front of them.
#[tokio::test]
#[serial]
async fn the_definition_carries_the_portal_calculation_a_member_was_computed_with() {
    let (db, app, token) = setup().await;
    let group = create_group(&app, &token, "pco2").await;
    let group_id = group["id"].as_str().unwrap().to_string();
    add_member(
        &app,
        &token,
        &group_id,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        0,
    )
    .await;
    let (_, text) = add_member(
        &app,
        &token,
        &group_id,
        crate::common::GLOBAL_PARAM_DO_ID,
        1,
    )
    .await;
    let computed = serde_json::from_str::<serde_json::Value>(&text).expect("member body is JSON");
    crate::common::exec(
        &db,
        &format!(
            "UPDATE parameter_group_members \
                SET source_calculation = '{{\"function\": \"calcPCO2\", \
                                            \"inputs\": [\"WTW_pH_1\", \"Field_BP\"]}}'::jsonb \
              WHERE id = '{}'",
            computed["id"].as_str().unwrap()
        ),
    )
    .await;

    let (status, text) = crate::common::get_with_token(
        &app,
        &format!("/api/parameter_groups/{group_id}/definition"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "definition ({status}): {text}");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("definition is JSON");
    let members = doc["members"].as_array().expect("members array");
    assert_eq!(
        members[1]["source_calculation"],
        json!({ "function": "calcPCO2", "inputs": ["WTW_pH_1", "Field_BP"] }),
        "the recorded portal calculation reaches the page: {text}"
    );
    assert!(
        members[0].get("source_calculation").is_none(),
        "a column nothing computed carries none: {text}"
    );
}

/// Scenario: a formula on a calculation bound to a group mints the parameter it publishes.
///
/// Expected behaviour: the parameter is minted and joins no group. A group is a filter over the
/// grid's columns, and a member is put there by a person in the group settings (Q189); a
/// membership nobody made also spends the one group a parameter is allowed.
#[tokio::test]
#[serial]
async fn a_minted_output_joins_no_group() {
    let (db, app, token) = setup().await;
    create_group(&app, &token, "dom_mint").await;
    let script_id = "00000000-0000-4000-c000-0000000002b1";
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO tool_scripts (id, name, label, engine, created_by) \
             VALUES ('{script_id}', 'dom_mint', 'DOM', 'formula', 'test')"
        ),
    )
    .await;

    let (status, text) = crate::common::post_json_with_token(
        &app,
        "/api/derived_parameters",
        &json!({
            "code": "dom_mint_out",
            "name": "DOM mint out",
            "units": "ratio",
            "formula": "Dissolved_O2 / DO_Temperature",
            "tool_script_id": script_id,
            "ordinal": 1,
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "add formula ({status}): {text}"
    );
    let created: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let output = created["output_parameter_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the formula publishes a parameter: {created}"));

    let members = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT count(*) AS n FROM parameter_group_members WHERE parameter_id = '{output}'"
            ),
        ))
        .await
        .expect("the membership reads")
        .expect("a count")
        .try_get::<i64>("", "n")
        .expect("n");
    assert_eq!(
        members, 0,
        "the minted output is in no group until somebody puts it in one"
    );
}
