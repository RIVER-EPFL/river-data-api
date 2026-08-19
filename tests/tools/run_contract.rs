//! The contract around a tool run: what the response records about how a number was produced,
//! how a script failure is reported, and how the manifest constrains a request.
//!
//! These tests need the OpenCPU runner on localhost:8006.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::keycloak as kc;

const PROBE: &str = "probe_contract";

const PROBE_SCRIPT: &str = r#"tool <- function(inputs, constants, curves) {
  if (isTRUE(inputs$explode)) stop("probe blew up")
  list(verbose_seen = if (isTRUE(inputs$verbose)) 1 else 0)
}"#;

fn probe_manifest() -> serde_json::Value {
    json!({
        "label": "Probe",
        "params": [
            { "name": "mode", "label": "Mode", "kind": "enum:simple|full_pipeline",
              "required": false, "default": "simple" },
            { "name": "stage", "label": "Stage", "kind": "enum:a|b|c",
              "required": false, "default": "a" },
            { "name": "secret", "label": "Secret", "kind": "number", "required": true,
              "when": { "param": "mode", "equals": "full_pipeline" } },
            { "name": "stage_note", "label": "Stage note", "kind": "number", "required": true,
              "when": { "param": "stage", "any_of": ["b", "c"] } },
            { "name": "legacy_note", "label": "Legacy note", "kind": "number", "required": true,
              "when": "mode=full_pipeline" },
            { "name": "verbose", "label": "Verbose", "kind": "boolean", "required": false,
              "default": false },
            { "name": "audited", "label": "Audited", "kind": "number", "required": true,
              "when": { "param": "verbose", "equals": true } },
            { "name": "explode", "label": "Explode", "kind": "boolean", "required": false }
        ],
        "outputs": [
            { "key": "verbose_seen", "label": "Verbose seen", "per_replicate": false }
        ]
    })
}

async fn install_probe_tool(db: &DatabaseConnection, manifest: &serde_json::Value) {
    remove_probe_tool(db).await;
    for statement in [
        Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO tool_scripts (name, label, created_by) VALUES ($1, 'Probe', 'test')",
            [PROBE.into()],
        ),
        Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"INSERT INTO tool_script_versions
                  (tool_script_id, version_no, script, entry_function, manifest, test_cases,
                   content_hash, created_by, validated_at)
              SELECT s.id, 1, $2, 'tool', $3::jsonb, '{}'::jsonb, md5($2), 'test', now()
              FROM tool_scripts s WHERE s.name = $1",
            [
                PROBE.into(),
                PROBE_SCRIPT.into(),
                manifest.to_string().into(),
            ],
        ),
        Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE tool_scripts s SET active_version_id = v.id
              FROM tool_script_versions v
              WHERE v.tool_script_id = s.id AND s.name = $1",
            [PROBE.into()],
        ),
    ] {
        db.execute(statement).await.expect("probe tool installed");
    }
}

async fn remove_probe_tool(db: &DatabaseConnection) {
    for sql in [
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name = $1",
        "DELETE FROM tool_scripts WHERE name = $1",
    ] {
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            [PROBE.into()],
        ))
        .await
        .expect("probe tool removed");
    }
}

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

async fn calculate(
    app: &axum::Router,
    tool: &str,
    payload: serde_json::Value,
    token: &str,
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        app,
        &format!("/api/tools/{tool}/calculate"),
        &payload,
        token,
    )
    .await
}

/// Expected behaviour: the values the server resolved travel back with the number, so the
/// provenance blob records what produced it rather than what the browser offered.
#[tokio::test]
#[serial]
async fn a_result_carries_the_constants_and_curves_the_server_resolved() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_result_carries_the_constants_and_curves_the_server_resolved",
    )
    .await
    {
        return;
    }
    let (_db, app, token) = setup().await;

    let (status, json) = calculate(
        &app,
        "doc",
        json!({
            "replicates": [120.0, 125.0, 118.0],
            "std_curve": { "slope": 1.05, "intercept": -2.0, "label": "bench curve" }
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{json}");

    let curves = json["curves"].as_array().expect("curves array");
    assert_eq!(curves.len(), 1, "{curves:?}");
    assert_eq!(curves[0]["name"], "std_curve");
    assert_eq!(curves[0]["curve"]["slope"], 1.05);
    assert_eq!(curves[0]["curve"]["intercept"], -2.0);
    assert_eq!(curves[0]["curve"]["label"], "bench curve");

    let (status, json) = calculate(
        &app,
        "pco2",
        json!({
            "water_temp_c": 25.0,
            "co2_ppm": 3774.31004084647,
            "h2o_percent": 3.04813831089996,
            "ch4_ppm": 476.103632267332,
            "lab_temp_c": 17.0519462262746,
            "lab_pressure_hpa": 1012.5826292476703
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{json}");

    let constants = json["constants"].as_object().expect("constants map");
    for name in [
        "c_const",
        "gas_const_r_atm",
        "gas_const_r_mol",
        "h_ch4_29815k",
        "ch4_in_sa",
        "lab_temp_avg_degC",
        "lab_press_avg_atm",
        "vol_sa",
        "vol_water",
    ] {
        assert!(
            constants
                .get(name)
                .is_some_and(serde_json::Value::is_number),
            "constant {name} missing from {constants:?}"
        );
    }
}

/// Expected behaviour: the runtime that executed the script is part of the result's identity.
#[tokio::test]
#[serial]
async fn a_result_pins_the_runner_that_executed_it() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_result_pins_the_runner_that_executed_it",
    )
    .await
    {
        return;
    }
    let (_db, app, token) = setup().await;

    let (status, json) = calculate(&app, "doc", json!({ "replicates": [120.0] }), &token).await;
    assert_eq!(status, 200, "{json}");

    let version = &json["tool_version"];
    assert!(version["content_hash"].is_string(), "{version}");
    assert!(
        version["r_version"]
            .as_str()
            .is_some_and(|v| v.contains("R version")),
        "the runner's R version is recorded: {version}"
    );
    assert!(
        version["runner_image"]
            .as_str()
            .is_some_and(|v| !v.is_empty()),
        "the runner's image build is recorded: {version}"
    );
}

/// Expected behaviour: a failure raised inside the script is reported field by field, so the
/// script editor can render the message apart from the call and the traceback.
#[tokio::test]
#[serial]
async fn a_script_failure_reports_message_call_and_traceback() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_script_failure_reports_message_call_and_traceback",
    )
    .await
    {
        return;
    }
    let (db, app, token) = setup().await;
    install_probe_tool(&db, &probe_manifest()).await;

    let (status, json) = calculate(&app, PROBE, json!({ "explode": true }), &token).await;
    remove_probe_tool(&db).await;

    assert_eq!(status, 400, "{json}");
    assert_eq!(json["message"], "probe blew up", "{json}");
    assert!(
        json["error"]
            .as_str()
            .is_some_and(|e| e.contains("probe blew up")),
        "{json}"
    );
    assert!(json["call"].is_string(), "the failing call: {json}");
    assert!(
        json["traceback"].as_array().is_some_and(|t| !t.is_empty()),
        "the traceback: {json}"
    );
}

/// Expected behaviour: a structured `when` gates the required check against the request; a
/// free-text `when` stays a note and gates nothing.
#[tokio::test]
#[serial]
async fn a_structured_when_gates_requiredness_and_a_note_does_not() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_structured_when_gates_requiredness_and_a_note_does_not",
    )
    .await
    {
        return;
    }
    let (db, app, token) = setup().await;
    install_probe_tool(&db, &probe_manifest()).await;

    let cases = vec![
        (json!({}), 200),
        (json!({ "mode": "simple" }), 200),
        (json!({ "mode": "full_pipeline" }), 400),
        (json!({ "mode": "full_pipeline", "secret": 1.0 }), 200),
        (json!({ "stage": "b" }), 400),
        (json!({ "stage": "b", "stage_note": 2.0 }), 200),
        (json!({ "stage": "a" }), 200),
    ];
    let mut got = Vec::new();
    for (payload, _) in &cases {
        got.push(calculate(&app, PROBE, payload.clone(), &token).await);
    }
    remove_probe_tool(&db).await;

    for ((payload, expected), (status, json)) in cases.iter().zip(&got) {
        assert_eq!(status, expected, "{payload} -> {json}");
    }
    // legacy_note is required behind a free-text note and is never sent, so a 200 above is
    // itself the proof that the note stays advisory.
    let (_, refused) = &got[2];
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|e| e.contains("secret")),
        "the refusal names the conditional param: {refused}"
    );
}

/// Expected behaviour: a boolean param accepts only JSON booleans, its default reaches the
/// script, and its value can gate another param's requiredness.
#[tokio::test]
#[serial]
async fn boolean_params_validate_default_and_gate() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "boolean_params_validate_default_and_gate",
    )
    .await
    {
        return;
    }
    let (db, app, token) = setup().await;
    install_probe_tool(&db, &probe_manifest()).await;

    let defaulted = calculate(&app, PROBE, json!({}), &token).await;
    let explicit = calculate(
        &app,
        PROBE,
        json!({ "verbose": true, "audited": 1.0 }),
        &token,
    )
    .await;
    let gated = calculate(&app, PROBE, json!({ "verbose": true }), &token).await;
    let mistyped = calculate(&app, PROBE, json!({ "verbose": "yes" }), &token).await;
    let numeric = calculate(&app, PROBE, json!({ "verbose": 1 }), &token).await;
    remove_probe_tool(&db).await;

    assert_eq!(defaulted.0, 200, "{:?}", defaulted.1);
    assert_eq!(
        defaulted.1["results"]["verbose_seen"], 0,
        "the default false reaches the script: {}",
        defaulted.1
    );
    assert_eq!(explicit.0, 200, "{:?}", explicit.1);
    assert_eq!(explicit.1["results"]["verbose_seen"], 1, "{}", explicit.1);
    assert_eq!(gated.0, 400, "verbose=true requires audited: {}", gated.1);
    assert_eq!(mistyped.0, 400, "{}", mistyped.1);
    assert_eq!(numeric.0, 400, "{}", numeric.1);
}

/// Expected behaviour: a structured param's declared columns are what its value may carry, so a
/// column the structure does not name, an entry-only column and a cell of the wrong arity are all
/// refused by name rather than reaching the script.
#[tokio::test]
#[serial]
async fn a_structured_param_is_checked_against_its_declaration() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_structured_param_is_checked_against_its_declaration",
    )
    .await
    {
        return;
    }
    let (_db, app, token) = setup().await;

    let row = |extra: serde_json::Value| {
        let mut base = json!({
            "fluor_before": 150.0, "vol_total_ml": 100.0, "vol_after_ml": 40.0,
            "diameters_cm": [10.0, 8.0, 6.0]
        });
        let (base_obj, extra_obj) = (base.as_object_mut().unwrap(), extra.as_object().unwrap());
        for (k, v) in extra_obj {
            base_obj.insert(k.clone(), v.clone());
        }
        json!({ "replicates": [base] })
    };

    let declared = calculate(&app, "chla_benthic", row(json!({})), &token).await;
    let undeclared = calculate(
        &app,
        "chla_benthic",
        row(json!({ "fluor_middle": 12.0 })),
        &token,
    )
    .await;
    let entry_only = calculate(
        &app,
        "chla_benthic",
        row(json!({ "wgt_dried_g": 0.02 })),
        &token,
    )
    .await;
    let wrong_arity = calculate(
        &app,
        "chla_benthic",
        row(json!({ "diameters_cm": 10.0 })),
        &token,
    )
    .await;
    // A flat replicate family is checked the same way: the declared cell is a number of its own,
    // and a list where a number is declared is refused rather than reaching the script.
    let flat_cell = calculate(&app, "nutrients", json!({ "NUT_TDP_rep_A": 7.0 }), &token).await;
    let flat_cell_wrong_arity = calculate(
        &app,
        "nutrients",
        json!({ "NUT_TDP_rep_A": [7.0, 8.0, 9.0] }),
        &token,
    )
    .await;

    assert_eq!(declared.0, 200, "{}", declared.1);
    for (name, (status, body)) in [
        ("fluor_middle", &undeclared),
        ("wgt_dried_g", &entry_only),
        ("diameters_cm", &wrong_arity),
    ] {
        assert_eq!(*status, 400, "{name}: {body}");
        assert!(
            body["error"].as_str().is_some_and(|e| e.contains(name)),
            "the refusal names the field: {body}"
        );
    }
    assert_eq!(flat_cell.0, 200, "{}", flat_cell.1);
    assert_eq!(flat_cell_wrong_arity.0, 400, "{}", flat_cell_wrong_arity.1);
}

/// Expected behaviour: the `kind` vocabulary is closed, so a manifest naming an unknown kind is
/// refused when the version is authored rather than accepting anything at call time.
#[tokio::test]
#[serial]
async fn an_unknown_kind_is_refused_at_authoring() {
    if !kc::require_keycloak_or_skip("an_unknown_kind_is_refused_at_authoring").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    kc::ensure_realm_user("kindadmin", "kindadmin", &["riverdata-admin"]).await;
    let admin = kc::get_keycloak_jwt("kindadmin", "kindadmin").await;

    let (status, script) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tool_scripts",
        &json!({ "name": "kind_probe", "label": "Kind probe" }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{script}");
    let sid = script["id"].as_str().unwrap().to_string();

    let version = |params: serde_json::Value| {
        json!({
            "script": "tool <- function(inputs, constants, curves) list(x = 1)",
            "manifest": { "label": "Kind probe", "params": params, "outputs": [] },
        })
    };
    let bad_kind = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &version(json!([{ "name": "hue", "label": "Hue", "kind": "colour" }])),
        &admin,
    )
    .await;
    let bad_default = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &version(
            json!([{ "name": "flag", "label": "Flag", "kind": "boolean", "default": "true" }]),
        ),
        &admin,
    )
    .await;
    let bad_when = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &version(
            json!([{ "name": "x", "label": "X", "kind": "number", "required": true,
                          "when": { "param": "nosuch", "equals": 1 } }]),
        ),
        &admin,
    )
    .await;

    let bad_structure = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &version(
            json!([{ "name": "rows", "label": "Rows", "kind": "replicate_grid",
                          "structure": { "fields": [
                              { "name": "afdm_g", "label": "AFDM",
                                "computed": { "subtract": ["dried_g", "ashed_g"] } }
                          ] } }]),
        ),
        &admin,
    )
    .await;

    crate::common::exec(
        &db,
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name = 'kind_probe'",
    )
    .await;
    crate::common::exec(&db, "DELETE FROM tool_scripts WHERE name = 'kind_probe'").await;

    assert_eq!(bad_kind.0, 400, "unknown kind: {}", bad_kind.1);
    assert!(
        bad_kind.1["error"]
            .as_str()
            .is_some_and(|e| e.contains("colour")),
        "the refusal names the kind: {}",
        bad_kind.1
    );
    assert_eq!(
        bad_default.0, 400,
        "default not a boolean: {}",
        bad_default.1
    );
    assert_eq!(bad_when.0, 400, "when names no param: {}", bad_when.1);
    assert_eq!(
        bad_structure.0, 400,
        "computed field names no field: {}",
        bad_structure.1
    );
}
