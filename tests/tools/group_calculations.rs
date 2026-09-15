//! The calculations of a parameter group. A calculation names no group (Q169): what ties the two
//! is the parameters they share, so two calculations over one group's members are ordinary and the
//! group's page lists both.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::keycloak as kc;

const GROUP_ID: &str = "00000000-0000-4000-c000-00000000c288";
const FIRST: &str = "group_calc_first";
const SECOND: &str = "group_calc_second";

async fn remove_script(db: &DatabaseConnection, name: &str) {
    for sql in [
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name = $1",
        "DELETE FROM tool_scripts WHERE name = $1",
    ] {
        db.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            [name.into()],
        ))
        .await
        .expect("script removed");
    }
}

/// An Administrator JWT over a database holding one group with the seeded temperature in it.
/// `tool_scripts` outlives the truncation, so both probe calculations clear their own rows first.
async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    for name in [FIRST, SECOND] {
        remove_script(&db, name).await;
    }
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO parameter_groups (id, code, label, ordinal) \
             VALUES ('{GROUP_ID}', 'shared', 'Shared', 1)"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal) \
             VALUES (gen_random_uuid(), '{GROUP_ID}', '{}', 1)",
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::member_jwt("groupadmin", "groupadmin", "riverdata-admin").await;
    (db, app, admin)
}

async fn create(app: &axum::Router, admin: &str, name: &str) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        app,
        "/api/tool_scripts",
        &json!({ "name": name, "label": name, "engine": "formula" }),
        admin,
    )
    .await
}

/// A version reading the group's temperature, activated so the calculation counts as one.
async fn activate_over_temperature(db: &DatabaseConnection, name: &str) {
    let manifest = json!({
        "label": name,
        "params": [{ "name": "temp", "kind": "number", "parameter_code": "DO_Temperature" }],
        "outputs": [],
    });
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO tool_script_versions \
               (id, tool_script_id, version_no, script, entry_function, manifest, test_cases, \
                content_hash, created_by) \
             SELECT gen_random_uuid(), s.id, 1, '', 'tool', '{payload}'::jsonb, '[]'::jsonb, \
                    'group-calc-{name}', 'test' \
               FROM tool_scripts s WHERE s.name = '{name}'",
            payload = manifest.to_string().replace('\'', "''")
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "UPDATE tool_scripts s SET active_version_id = v.id \
               FROM tool_script_versions v \
              WHERE v.tool_script_id = s.id AND s.name = '{name}'"
        ),
    )
    .await;
}

#[tokio::test]
#[serial]
async fn two_calculations_may_read_one_group_and_the_group_lists_both() {
    let (db, app, admin) = setup().await;

    for name in [FIRST, SECOND] {
        let (status, body) = create(&app, &admin, name).await;
        assert_eq!(status, 200, "{name} created: {body}");
        activate_over_temperature(&db, name).await;
    }

    let (status, text) = crate::common::get_with_token(
        &app,
        &format!("/api/parameter_groups/{GROUP_ID}/definition"),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "definition ({status}): {text}");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let listed: Vec<&str> = doc["calculations"]
        .as_array()
        .expect("calculations")
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert!(
        listed.contains(&FIRST) && listed.contains(&SECOND),
        "both calculations read the group's temperature: {text}"
    );
}

#[tokio::test]
#[serial]
async fn a_calculation_reading_none_of_the_members_is_not_one_of_the_group_s() {
    let (db, app, admin) = setup().await;

    let (status, body) = create(&app, &admin, FIRST).await;
    assert_eq!(status, 200, "{body}");
    let manifest = json!({
        "label": FIRST,
        "params": [{ "name": "o", "kind": "number", "parameter_code": "Dissolved_O2" }],
        "outputs": [],
    });
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO tool_script_versions \
               (id, tool_script_id, version_no, script, entry_function, manifest, test_cases, \
                content_hash, created_by) \
             SELECT gen_random_uuid(), s.id, 1, '', 'tool', '{payload}'::jsonb, '[]'::jsonb, \
                    'group-calc-other', 'test' \
               FROM tool_scripts s WHERE s.name = '{FIRST}'",
            payload = manifest.to_string().replace('\'', "''")
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE tool_scripts s SET active_version_id = v.id \
               FROM tool_script_versions v \
              WHERE v.tool_script_id = s.id AND s.name = '{FIRST}'"
        ),
    )
    .await;

    let (status, text) = crate::common::get_with_token(
        &app,
        &format!("/api/parameter_groups/{GROUP_ID}/definition"),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(
        doc["calculations"].as_array().expect("calculations").len(),
        0,
        "it reads oxygen, which this group does not hold: {text}"
    );
}
