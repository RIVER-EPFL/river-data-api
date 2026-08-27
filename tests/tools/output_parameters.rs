//! An output names the catalog parameter it is saved to, and the server resolves it: id first,
//! then code, case-insensitively. What the authoring surface refuses, and what it reports.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::fixtures::{GLOBAL_PARAM_COND_ID, GLOBAL_PARAM_DO_ID, GLOBAL_PARAM_TEMP_ID};
use crate::common::keycloak as kc;

const ABSENT_PARAMETER_ID: &str = "00000000-0000-4000-c000-0000000000ff";
const DELETED_PARAMETER_ID: &str = "00000000-0000-4000-c000-0000000000fe";
const REPLACEMENT_PARAMETER_ID: &str = "00000000-0000-4000-c000-0000000000fd";

async fn insert_parameter(db: &DatabaseConnection, id: &str, code: &str) {
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
             VALUES ('{id}', '{code}', 'Dangling probe', 'ppb', 'measurement')"
        ),
    ))
    .await
    .expect("parameter inserted");
}

async fn remove_script(db: &DatabaseConnection, name: &str) {
    for sql in [
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name = $1",
        "DELETE FROM tool_scripts WHERE name = $1",
    ] {
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            [name.into()],
        ))
        .await
        .expect("script removed");
    }
}

async fn setup(script_name: &str) -> (axum::Router, String, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    remove_script(&db, script_name).await;
    let app = kc::build_test_app_with_keycloak(db).await;
    kc::ensure_realm_user("scriptadmin", "scriptadmin", &["riverdata-admin"]).await;
    let admin = kc::get_keycloak_jwt("scriptadmin", "scriptadmin").await;

    let (status, script) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tool_scripts",
        &json!({ "name": script_name, "label": "Output catalog probe" }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "script created: {script}");
    let id = script["id"].as_str().unwrap().to_string();
    (app, admin, id)
}

/// `tool_scripts` outlives the truncation, so a probe left behind would count as a tool in every
/// later test.
async fn teardown(script_name: &str) {
    let db = crate::common::setup_test_db().await;
    remove_script(&db, script_name).await;
}

const DOUBLER: &str = "tool <- function(inputs, constants, curves) list(doubled = 2 * inputs$x)";

fn manifest(outputs: serde_json::Value) -> serde_json::Value {
    json!({
        "label": "Output catalog probe",
        "params": [ { "name": "x", "label": "X", "kind": "number", "required": true } ],
        "outputs": outputs,
    })
}

fn cases() -> serde_json::Value {
    json!({ "tolerance": 1e-9, "cases": [
        { "name": "two", "inputs": { "x": 2 }, "expected": { "doubled": 4 } } ] })
}

/// Attempt a version, whatever the authoring surface makes of it.
async fn create_version(
    app: &axum::Router,
    admin: &str,
    sid: &str,
    manifest: &serde_json::Value,
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &json!({ "script": DOUBLER, "manifest": manifest, "test_cases": cases() }),
        admin,
    )
    .await
}

async fn activate(
    app: &axum::Router,
    admin: &str,
    sid: &str,
    vid: &str,
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        app,
        &format!("/api/tool_scripts/{sid}/versions/{vid}/activate"),
        &json!({}),
        admin,
    )
    .await
}

/// Create a version, validate it and activate it, so `GET /tools` serves this manifest.
async fn publish(
    app: &axum::Router,
    admin: &str,
    sid: &str,
    manifest: &serde_json::Value,
) -> String {
    let (status, version) = create_version(app, admin, sid, manifest).await;
    assert_eq!(status, 200, "version created: {version}");
    let vid = version["version"]["id"].as_str().unwrap().to_string();
    for step in ["validate", "activate"] {
        let (status, body) = crate::common::post_json_parse_with_token(
            app,
            &format!("/api/tool_scripts/{sid}/versions/{vid}/{step}"),
            &json!({}),
            admin,
        )
        .await;
        assert_eq!(status, 200, "{step}: {body}");
    }
    vid
}

async fn served_outputs(app: &axum::Router, admin: &str, name: &str) -> serde_json::Value {
    let (status, tools) = crate::common::get_json_with_token(app, "/api/tools", admin).await;
    assert_eq!(status, 200, "tools listed: {tools}");
    tools
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("{name} is served: {tools}"))["outputs"]
        .clone()
}

fn output<'a>(outputs: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
    outputs
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["key"] == key)
        .unwrap_or_else(|| panic!("output {key} is served: {outputs}"))
}

#[tokio::test]
#[serial]
async fn an_output_resolves_by_id_then_by_code() {
    if !crate::common::tools_runner::require_runner_or_skip("an_output_resolves_by_id_then_by_code")
        .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_output_resolution").await {
        return;
    }
    let (app, admin, sid) = setup("output_resolution").await;

    publish(
        &app,
        &admin,
        &sid,
        &manifest(json!([
            { "key": "by_id", "label": "By id", "parameter_id": GLOBAL_PARAM_TEMP_ID },
            { "key": "both_halves", "label": "Both halves", "parameter_id": GLOBAL_PARAM_DO_ID,
              "suggested_parameter_code": "dissolved_o2" },
            { "key": "by_code", "label": "By code", "suggested_parameter_code": "Conductivity" },
            { "key": "other_case", "label": "Other case",
              "suggested_parameter_code": "TURBIDITY" },
            { "key": "doubled", "label": "Doubled" }
        ])),
    )
    .await;

    let outputs = served_outputs(&app, &admin, "output_resolution").await;

    let by_id = output(&outputs, "by_id");
    assert_eq!(by_id["parameter"]["id"], GLOBAL_PARAM_TEMP_ID, "{by_id}");
    assert_eq!(by_id["parameter"]["code"], "DO_Temperature", "{by_id}");
    assert_eq!(by_id["parameter"]["name"], "Water Temperature", "{by_id}");
    assert_eq!(by_id["parameter"]["default_units"], "°C", "{by_id}");
    assert_eq!(by_id["parameter"]["needs_review"], false, "{by_id}");
    assert_eq!(by_id["parameter"]["resolved_by"], "id", "{by_id}");
    assert_eq!(
        by_id["suggested_parameter_code"], "DO_Temperature",
        "the resolved code is stamped into the stored manifest: {by_id}"
    );
    assert_eq!(
        by_id["parameter"]["dangling_parameter_id"], false,
        "{by_id}"
    );

    let both_halves = output(&outputs, "both_halves");
    assert_eq!(
        both_halves["parameter"]["id"], GLOBAL_PARAM_DO_ID,
        "{both_halves}"
    );
    assert_eq!(
        both_halves["parameter"]["resolved_by"], "id",
        "{both_halves}"
    );
    assert_eq!(
        both_halves["suggested_parameter_code"], "dissolved_o2",
        "an agreeing code is served as authored, case and all: {both_halves}"
    );

    let by_code = output(&outputs, "by_code");
    assert_eq!(by_code["parameter"]["code"], "Conductivity", "{by_code}");
    assert_eq!(by_code["parameter"]["resolved_by"], "code", "{by_code}");

    let other_case = output(&outputs, "other_case");
    assert_eq!(other_case["parameter"]["code"], "Turbidity", "{other_case}");

    let doubled = output(&outputs, "doubled");
    assert!(
        doubled["parameter"].is_null(),
        "an output naming no parameter resolves to nothing: {doubled}"
    );

    teardown("output_resolution").await;
}

#[tokio::test]
#[serial]
async fn an_unknown_id_is_refused_and_an_unknown_code_is_reported() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "an_unknown_id_is_refused_and_an_unknown_code_is_reported",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_output_catalog_findings").await {
        return;
    }
    let (app, admin, sid) = setup("output_catalog_findings").await;
    let versions = format!("/api/tool_scripts/{sid}/versions");

    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        &versions,
        &json!({
            "script": DOUBLER,
            "manifest": manifest(json!([
                { "key": "doubled", "label": "Doubled",
                  "parameter_id": ABSENT_PARAMETER_ID }
            ])),
        }),
        &admin,
    )
    .await;
    assert_eq!(
        status, 409,
        "an id no parameter carries is refused: {refused}"
    );
    let detail = refused["detail"].to_string();
    assert!(
        detail.contains("doubled"),
        "the output key is named: {detail}"
    );
    assert!(
        detail.contains(ABSENT_PARAMETER_ID),
        "the offending id is named: {detail}"
    );

    let (status, created) = crate::common::post_json_parse_with_token(
        &app,
        &versions,
        &json!({
            "script": DOUBLER,
            "manifest": manifest(json!([
                { "key": "doubled", "label": "Doubled",
                  "suggested_parameter_code": "Chlorophyll_a" }
            ])),
        }),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "a code the catalog does not hold yet still stores: {created}"
    );
    let lint = created["lint"].to_string();
    assert!(
        lint.contains("doubled") && lint.contains("Chlorophyll_a"),
        "the code is reported rather than passing silently: {lint}"
    );

    teardown("output_catalog_findings").await;
}

/// Elsewhere the id names another analyte or nothing at all, so the two halves cannot both be what
/// the output saves to and no stamping can decide which was meant.
#[tokio::test]
#[serial]
async fn an_id_and_a_code_naming_different_parameters_are_refused() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "an_id_and_a_code_naming_different_parameters_are_refused",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_output_disagreement").await {
        return;
    }
    let (app, admin, sid) = setup("output_disagreement").await;

    let (status, refused) = create_version(
        &app,
        &admin,
        &sid,
        &manifest(json!([
            { "key": "doubled", "label": "Doubled", "parameter_id": GLOBAL_PARAM_TEMP_ID,
              "suggested_parameter_code": "Conductivity" }
        ])),
    )
    .await;
    assert_eq!(status, 409, "the disagreement is refused: {refused}");
    let detail = refused["detail"].to_string();
    for named in [
        "doubled",
        GLOBAL_PARAM_TEMP_ID,
        "DO_Temperature",
        "Conductivity",
    ] {
        assert!(
            detail.contains(named),
            "the output key, the id and both codes are named ({named} missing): {detail}"
        );
    }

    teardown("output_disagreement").await;
}

/// The collision is on the resolved parameter, not on the spelling: one output names the id and
/// the other the code of that same row.
#[tokio::test]
#[serial]
async fn two_outputs_resolving_to_one_parameter_are_refused() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "two_outputs_resolving_to_one_parameter_are_refused",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_output_collision").await {
        return;
    }
    let (app, admin, sid) = setup("output_collision").await;

    let (status, refused) = create_version(
        &app,
        &admin,
        &sid,
        &manifest(json!([
            { "key": "doubled", "label": "Doubled", "parameter_id": GLOBAL_PARAM_COND_ID },
            { "key": "doubled_again", "label": "Doubled again",
              "suggested_parameter_code": "conductivity" }
        ])),
    )
    .await;
    assert_eq!(status, 409, "the collision is refused: {refused}");
    let detail = refused["detail"].to_string();
    for named in ["doubled", "doubled_again", "Conductivity"] {
        assert!(
            detail.contains(named),
            "both output keys and the parameter are named ({named} missing): {detail}"
        );
    }

    teardown("output_collision").await;
}

/// A parameter deleted after the version was saved leaves the authoritative half dead. Resolution
/// falls through to the code, which is what keeps the tool working, and the dead half is reported
/// rather than left for someone to notice.
#[tokio::test]
#[serial]
async fn a_dangling_parameter_id_is_flagged_and_reported_at_activation() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_dangling_parameter_id_is_flagged_and_reported_at_activation",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_output_dangling_id").await {
        return;
    }
    let (app, admin, sid) = setup("output_dangling_id").await;
    let db = crate::common::setup_test_db().await;
    insert_parameter(&db, DELETED_PARAMETER_ID, "dangling_probe").await;

    let vid = publish(
        &app,
        &admin,
        &sid,
        &manifest(json!([
            { "key": "doubled", "label": "Doubled", "parameter_id": DELETED_PARAMETER_ID }
        ])),
    )
    .await;

    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("DELETE FROM parameters WHERE id = '{DELETED_PARAMETER_ID}'"),
    ))
    .await
    .expect("parameter deleted");
    insert_parameter(&db, REPLACEMENT_PARAMETER_ID, "dangling_probe").await;

    let doubled = output(
        &served_outputs(&app, &admin, "output_dangling_id").await,
        "doubled",
    )
    .clone();
    assert_eq!(
        doubled["parameter"]["id"], REPLACEMENT_PARAMETER_ID,
        "the stamped code carries the output: {doubled}"
    );
    assert_eq!(doubled["parameter"]["resolved_by"], "code", "{doubled}");
    assert_eq!(
        doubled["parameter"]["dangling_parameter_id"], true,
        "the dead authoritative half is flagged: {doubled}"
    );

    let (status, activated) = activate(&app, &admin, &sid, &vid).await;
    assert_eq!(
        status, 200,
        "a version that still resolves activates: {activated}"
    );
    let lint = activated["lint"].to_string();
    assert!(
        lint.contains("doubled") && lint.contains(DELETED_PARAMETER_ID),
        "activation reports the dead id: {lint}"
    );

    teardown("output_dangling_id").await;
}

/// The parse error has to point somewhere: without the path an editor is told only that a UUID
/// failed to parse, in a manifest that may carry twenty outputs.
#[tokio::test]
#[serial]
async fn a_malformed_parameter_id_names_the_output_it_sits_on() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_malformed_parameter_id_names_the_output_it_sits_on",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_output_malformed_id").await {
        return;
    }
    let (app, admin, sid) = setup("output_malformed_id").await;

    let (status, refused) = create_version(
        &app,
        &admin,
        &sid,
        &manifest(json!([
            { "key": "doubled", "label": "Doubled" },
            { "key": "tripled", "label": "Tripled", "parameter_id": "not-a-uuid" }
        ])),
    )
    .await;
    assert_eq!(status, 400, "an unreadable manifest is refused: {refused}");
    assert!(
        refused.to_string().contains("outputs[1].parameter_id"),
        "the field that was refused is named: {refused}"
    );

    teardown("output_malformed_id").await;
}

#[tokio::test]
#[serial]
async fn a_manifest_declaring_a_nonexistent_constant_is_refused() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_manifest_declaring_a_nonexistent_constant_is_refused",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_constant_findings").await {
        return;
    }
    let (app, admin, sid) = setup("constant_findings").await;

    let mut declared = manifest(json!([{ "key": "doubled", "label": "Doubled" }]));
    declared["constants"] = json!(["molar_mass_of_nothing"]);
    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &json!({ "script": DOUBLER, "manifest": declared }),
        &admin,
    )
    .await;
    assert_eq!(
        status, 409,
        "the constant cannot resolve at call time: {refused}"
    );
    assert!(
        refused["detail"]
            .to_string()
            .contains("molar_mass_of_nothing"),
        "the constant is named: {refused}"
    );

    teardown("constant_findings").await;
}
