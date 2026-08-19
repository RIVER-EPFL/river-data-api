//! `POST /api/tool_scripts/draft_run`: executing editor content that is not stored, through the
//! manifest handling a real calculate applies.
//!
//! These tests need the OpenCPU runner on localhost:8006.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::keycloak as kc;

/// A `parameter_id` no catalog row holds, so the manifest carries a finding of its own.
const DANGLING_PARAMETER_ID: &str = "00000000-0000-0000-0000-0000000000ff";

const DRAFT: &str = r"tool <- function(inputs, constants, curves) {
  list(doubled = 2 * inputs$x, slope_seen = curves$std$slope, c_seen = constants$c_const)
}";

fn manifest() -> serde_json::Value {
    json!({
        "label": "Draft probe",
        "params": [ { "name": "x", "label": "X", "kind": "number", "required": true } ],
        "outputs": [ { "key": "doubled", "label": "Doubled", "per_replicate": false } ],
        "constants": ["c_const"],
        "curves": [ { "name": "std", "label": "Standard curve", "required": true } ],
    })
}

fn inputs() -> serde_json::Value {
    json!({ "x": 2, "std": { "slope": 1.5, "intercept": 0.5, "label": "bench curve" } })
}

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    kc::ensure_realm_user("scriptadmin", "scriptadmin", &["riverdata-admin"]).await;
    let admin = kc::get_keycloak_jwt("scriptadmin", "scriptadmin").await;
    (db, app, admin)
}

async fn draft_run(
    app: &axum::Router,
    payload: &serde_json::Value,
    admin: &str,
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(app, "/api/tool_scripts/draft_run", payload, admin)
        .await
}

/// Row counts of everything the authoring surface writes.
async fn authoring_rows(db: &DatabaseConnection) -> Vec<i64> {
    let mut counts = Vec::new();
    for table in [
        "tool_scripts",
        "tool_script_versions",
        "tool_script_activations",
    ] {
        let row = db
            .query_one(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!("SELECT count(*) AS n FROM {table}"),
            ))
            .await
            .expect("count query")
            .expect("count row");
        counts.push(row.try_get("", "n").expect("count value"));
    }
    counts
}

#[tokio::test]
#[serial]
async fn a_draft_runs_against_the_catalog_and_stores_nothing() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_draft_runs_against_the_catalog_and_stores_nothing",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_draft_run").await {
        return;
    }
    let (db, app, admin) = setup().await;
    let before = authoring_rows(&db).await;

    let (status, out) = draft_run(
        &app,
        &json!({ "script": DRAFT, "manifest": manifest(), "inputs": inputs() }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "the draft ran: {out}");

    assert_eq!(out["ran"], true, "{out}");
    assert!(out["failure"].is_null(), "{out}");
    assert_eq!(out["results"]["doubled"], 4.0, "{out}");
    assert_eq!(
        out["results"]["slope_seen"], 1.5,
        "the curve reached the script: {out}"
    );
    let resolved_constant = out["constants"]["c_const"]
        .as_f64()
        .unwrap_or_else(|| panic!("the constant resolved from the catalog: {out}"));
    assert_eq!(
        out["results"]["c_seen"].as_f64(),
        Some(resolved_constant),
        "the script saw the value the server resolved: {out}"
    );

    let curves = out["curves"].as_array().expect("curves array");
    assert_eq!(curves.len(), 1, "{curves:?}");
    assert_eq!(curves[0]["name"], "std");
    assert_eq!(curves[0]["curve"]["label"], "bench curve");
    assert_eq!(out["inputs_used"], json!(["x", "std"]), "{out}");
    assert_eq!(out["inputs_ignored"], json!([]), "{out}");
    assert!(
        out["lint"].as_array().expect("lint array").is_empty(),
        "{out}"
    );

    let version = &out["tool_version"];
    assert!(version["content_hash"].is_string(), "{version}");
    assert!(
        version["script_version_id"].is_null() && version["version_no"].is_null(),
        "a draft has no stored version identity: {version}"
    );
    assert!(
        version["r_version"].as_str().is_some_and(|v| !v.is_empty()),
        "the runner that produced the number is recorded: {version}"
    );

    assert_eq!(
        authoring_rows(&db).await,
        before,
        "a draft run writes nothing"
    );
}

/// The manifest applies to a draft body exactly as it does to a calculate body; what differs is
/// that the refusal is reported at 200, next to the findings, rather than ending the response.
#[tokio::test]
#[serial]
async fn a_draft_body_the_manifest_refuses_is_reported_with_the_findings() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_draft_body_the_manifest_refuses_is_reported_with_the_findings",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_draft_run_manifest").await {
        return;
    }
    let (_db, app, admin) = setup().await;

    for (label, body, wanted) in [
        (
            "unknown field",
            json!({ "x": 2, "y": 3, "std": { "slope": 1.0, "intercept": 0.0 } }),
            "unknown field 'y'",
        ),
        (
            "wrong kind",
            json!({ "x": "two", "std": { "slope": 1.0, "intercept": 0.0 } }),
            "must be number",
        ),
        (
            "missing required param",
            json!({ "std": { "slope": 1.0, "intercept": 0.0 } }),
            "missing required field 'x'",
        ),
        (
            "missing required curve",
            json!({ "x": 2 }),
            "missing required curve 'std'",
        ),
    ] {
        let (status, out) = draft_run(
            &app,
            &json!({ "script": DRAFT, "manifest": manifest(), "inputs": body }),
            &admin,
        )
        .await;
        assert_eq!(status, 200, "{label}: {out}");
        assert_eq!(out["ran"], false, "{label}: {out}");
        assert!(
            out["results"].is_null(),
            "{label}: no results are invented for a body that never ran: {out}"
        );
        assert_eq!(out["failure"]["kind"], "body_refused", "{label}: {out}");
        assert!(
            out["failure"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains(wanted),
            "{label} is refused by name: {out}"
        );
    }
}

/// A manifest that cannot be read leaves nothing to report findings about, so it stays a refusal
/// and names the field it choked on.
#[tokio::test]
#[serial]
async fn an_unreadable_draft_manifest_is_refused_by_path() {
    if !kc::require_keycloak_or_skip("tool_script_draft_run_bad_manifest").await {
        return;
    }
    let (_db, app, admin) = setup().await;

    let (status, out) = draft_run(
        &app,
        &json!({
            "script": DRAFT,
            "manifest": { "label": "Bad", "params": [
                { "name": "x", "label": "X", "kind": "colour" } ] },
            "inputs": { "x": 2 },
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 400, "an unreadable manifest is refused: {out}");
    let message = out["error"].as_str().unwrap_or_default();
    assert!(message.contains("unknown kind 'colour'"), "{out}");
    assert!(
        message.contains("params[0]"),
        "the refused field is named: {out}"
    );
}

/// The two halves an author needs at once: what the script did, and what the manifest says.
#[tokio::test]
#[serial]
async fn a_draft_whose_script_raises_reports_the_failure_and_the_findings_together() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_draft_whose_script_raises_reports_the_failure_and_the_findings_together",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_draft_run_script_error").await {
        return;
    }
    let (_db, app, admin) = setup().await;

    let (status, out) = draft_run(
        &app,
        &json!({
            "script": "tool <- function(inputs, constants, curves) stop(\"the draft blew up\")",
            "manifest": {
                "label": "Raising draft",
                "params": [ { "name": "x", "label": "X", "kind": "number", "required": true } ],
                "outputs": [ { "key": "doubled", "label": "Doubled", "per_replicate": false,
                               "parameter_id": DANGLING_PARAMETER_ID } ],
            },
            "inputs": { "x": 2 },
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["ran"], false, "{out}");
    assert_eq!(out["failure"]["kind"], "script_error", "{out}");
    assert!(
        out["failure"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("the draft blew up"),
        "the R message survives: {out}"
    );
    assert!(
        !out["failure"]["traceback"]
            .as_array()
            .expect("traceback array")
            .is_empty(),
        "the R traceback survives: {out}"
    );

    let findings = out["lint"].as_array().expect("lint array");
    assert!(
        findings.iter().any(|f| f["message"]
            .as_str()
            .unwrap_or_default()
            .contains(DANGLING_PARAMETER_ID)),
        "the manifest finding is reported although the run failed: {out}"
    );
}

/// Deterministic without touching the runner container: the config points at a port nothing
/// listens on, so the connection is refused rather than hanging.
#[tokio::test]
#[serial]
async fn a_draft_against_an_absent_runner_reports_it_with_the_findings() {
    if !kc::require_keycloak_or_skip("tool_script_draft_run_absent_runner").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port");
        listener.local_addr().expect("the bound address").port()
    };
    let app = kc::build_test_app_with_keycloak_and_runner(
        db.clone(),
        Some(format!("http://127.0.0.1:{port}/ocpu")),
    )
    .await;
    kc::ensure_realm_user("scriptadmin", "scriptadmin", &["riverdata-admin"]).await;
    let admin = kc::get_keycloak_jwt("scriptadmin", "scriptadmin").await;

    let (status, out) = draft_run(
        &app,
        &json!({
            "script": DRAFT,
            "manifest": {
                "label": "Absent runner draft",
                "params": [ { "name": "x", "label": "X", "kind": "number", "required": true } ],
                "outputs": [ { "key": "doubled", "label": "Doubled", "per_replicate": false,
                               "parameter_id": DANGLING_PARAMETER_ID } ],
            },
            "inputs": { "x": 2 },
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["ran"], false, "{out}");
    assert_eq!(out["failure"]["kind"], "runner_unavailable", "{out}");
    assert!(
        out["failure"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("runner"),
        "the message names the runner: {out}"
    );
    assert!(
        out["lint"]
            .as_array()
            .expect("lint array")
            .iter()
            .any(|f| f["message"]
                .as_str()
                .unwrap_or_default()
                .contains(DANGLING_PARAMETER_ID)),
        "the manifest finding is reported although the runner is absent: {out}"
    );
}

/// The forbidden call sits in a branch that never executes, so the run still produces a number
/// and the finding travels with it.
#[tokio::test]
#[serial]
async fn a_draft_reports_its_lint_findings_alongside_the_result() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_draft_reports_its_lint_findings_alongside_the_result",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_draft_run_lint").await {
        return;
    }
    let (_db, app, admin) = setup().await;

    let script = r#"tool <- function(inputs, constants, curves) {
  if (FALSE) unlink("scratch")
  list(doubled = 2 * inputs$x)
}"#;
    let (status, out) = draft_run(
        &app,
        &json!({
            "script": script,
            "manifest": { "label": "Lint draft", "params": [
                { "name": "x", "label": "X", "kind": "number", "required": true } ] },
            "inputs": { "x": 2 },
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["results"]["doubled"], 4.0, "{out}");

    let findings = out["lint"].as_array().expect("lint array");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0]["line"], 2, "{findings:?}");
    assert!(
        findings[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("'unlink'"),
        "{findings:?}"
    );
}

/// A constant name being written is as likely half-typed as deleted, so the draft runs without it
/// and says so. Ending the run instead would withhold the numbers and the finding together.
#[tokio::test]
#[serial]
async fn a_draft_naming_an_unknown_constant_runs_and_reports_it() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_draft_naming_an_unknown_constant_runs_and_reports_it",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_draft_run_unknown_constant").await {
        return;
    }
    let (_db, app, admin) = setup().await;

    let (status, out) = draft_run(
        &app,
        &json!({
            "script": "tool <- function(inputs, constants, curves) list(doubled = 2 * inputs$x)",
            "manifest": {
                "label": "Constant draft",
                "params": [ { "name": "x", "label": "X", "kind": "number", "required": true } ],
                "constants": ["molar_mass_of_nothing"],
            },
            "inputs": { "x": 2 },
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "the draft ran: {out}");
    assert_eq!(out["results"]["doubled"], 4.0, "{out}");
    assert!(
        out["constants"]["molar_mass_of_nothing"].is_null(),
        "no value was invented for it: {out}"
    );

    let findings = out["lint"].as_array().expect("lint array");
    let message = findings
        .iter()
        .find_map(|f| f["message"].as_str())
        .unwrap_or_default();
    assert!(
        message.contains("molar_mass_of_nothing"),
        "the constant is named: {findings:?}"
    );
    assert!(
        !message.contains("restore"),
        "the message fits a script being written, not a catalog that lost a row: {message}"
    );
}

#[tokio::test]
#[serial]
async fn no_api_token_reaches_the_draft_runner() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db);

    let (status, out) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tool_scripts/draft_run",
        &json!({ "script": DRAFT, "manifest": manifest(), "inputs": inputs() }),
        &token,
    )
    .await;
    assert!(
        status == 401 || status == 403,
        "a full-permission token is still not an Administrator ({status}): {out}"
    );
}
