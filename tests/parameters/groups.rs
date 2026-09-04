//! Parameter groups: the rules SQL holds, and the definition document the grid renders from.
//!
//! The move and delete decisions themselves are unit-tested in
//! `routes::private::parameters::groups::rules`; what only real SQL can show is the unique
//! membership, the role CHECK, the history trigger firing on every writer, and the document the
//! endpoint assembles from a group and the catalog.

use sea_orm::{ConnectionTrait, Statement};
use serde_json::json;
use serial_test::serial;

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
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
    role: &str,
    ordinal: i32,
) -> (u16, String) {
    crate::common::post_json_with_token(
        app,
        "/api/parameter_group_members",
        &json!({
            "group_id": group_id,
            "parameter_id": parameter_id,
            "role": role,
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
            "SELECT count(*) AS n FROM parameter_group_history WHERE group_id = $1::uuid",
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

    let (status, text) = add_member(
        &app,
        &token,
        dom_id,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "measured",
        0,
    )
    .await;
    assert!((200..300).contains(&status), "first add ({status}): {text}");

    let (status, text) = add_member(
        &app,
        &token,
        ions_id,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "measured",
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
async fn a_role_outside_the_three_is_refused() {
    let (_db, app, token) = setup().await;
    let group = create_group(&app, &token, "dom").await;
    let (status, text) = add_member(
        &app,
        &token,
        group["id"].as_str().unwrap(),
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "derived",
        0,
    )
    .await;
    assert_eq!(status, 400, "unknown role should be refused: {text}");
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
        "measured",
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
        "measured",
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
        &json!({ "role": "entry_only" }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "update ({status}): {text}");

    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT change, old->>'role' AS old_role, new->>'role' AS new_role \
               FROM parameter_group_history \
              WHERE group_id = $1::uuid AND change = 'member_update'",
            [group_id.as_str().into()],
        ))
        .await
        .expect("history query")
        .expect("an update row");
    let old_role: Option<String> = row.try_get("", "old_role").unwrap();
    let new_role: Option<String> = row.try_get("", "new_role").unwrap();
    assert_eq!(old_role.as_deref(), Some("measured"));
    assert_eq!(new_role.as_deref(), Some("entry_only"));
}

#[tokio::test]
#[serial]
async fn the_definition_document_is_the_group_in_its_own_order() {
    let (_db, app, token) = setup().await;
    let group = create_group(&app, &token, "dom").await;
    let group_id = group["id"].as_str().unwrap().to_string();

    add_member(
        &app,
        &token,
        &group_id,
        crate::common::GLOBAL_PARAM_DO_ID,
        "output",
        2,
    )
    .await;
    add_member(
        &app,
        &token,
        &group_id,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "measured",
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
    assert_eq!(members[0]["role"], "measured");
    assert_eq!(
        members[1]["parameter_id"],
        crate::common::GLOBAL_PARAM_DO_ID
    );
    assert_eq!(members[1]["role"], "output");
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
        "measured",
        1,
    )
    .await;
    add_member(
        &app,
        &token,
        &group_id,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "measured",
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
            "INSERT INTO tool_scripts (id, name, label, engine, parameter_group_id, created_by) \
             VALUES ('{script_id}', 'dom_sections', 'DOM', 'script', '{group_id}', 'test')"
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
