//! `POST /api/tool_scripts/inspect`: reading a script's inputs, constants, curve slots and
//! outputs off its parse tree, and setting them against a manifest.
//!
//! These tests need the OpenCPU runner on localhost:8006.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::keycloak as kc;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    kc::ensure_realm_user("scriptadmin", "scriptadmin", &["riverdata-admin"]).await;
    let admin = kc::get_keycloak_jwt("scriptadmin", "scriptadmin").await;
    (db, app, admin)
}

/// The R source and entry function of a seeded tool's active version.
async fn active_script(db: &DatabaseConnection, name: &str) -> (String, String) {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT v.script, v.entry_function
              FROM tool_scripts s JOIN tool_script_versions v ON v.id = s.active_version_id
              WHERE s.name = $1",
            [name.into()],
        ))
        .await
        .expect("query ran")
        .unwrap_or_else(|| panic!("tool '{name}' is seeded and active"));
    (
        row.try_get("", "script").expect("script"),
        row.try_get("", "entry_function").expect("entry"),
    )
}

async fn inspect(
    app: &axum::Router,
    payload: &serde_json::Value,
    admin: &str,
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(app, "/api/tool_scripts/inspect", payload, admin)
        .await
}

fn names(value: &serde_json::Value, field: &str) -> Vec<String> {
    value[field]
        .as_array()
        .unwrap_or_else(|| panic!("{field} is an array: {value}"))
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
#[serial]
async fn a_seeded_script_reports_the_inputs_and_curve_slots_it_reads() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_seeded_script_reports_the_inputs_and_curve_slots_it_reads",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_inspect_seeded").await {
        return;
    }
    let (db, app, admin) = setup().await;

    let (script, entry) = active_script(&db, "tss_afdm").await;
    let (status, out) = inspect(
        &app,
        &json!({ "script": script, "entry_function": entry }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["parse_ok"], true, "{out}");
    assert_eq!(out["entry_found"], true, "{out}");
    assert_eq!(
        names(&out, "entry_args"),
        vec!["inputs", "constants", "curves"]
    );
    assert_eq!(
        names(&out, "inputs"),
        vec![
            "vol_filtered_ml",
            "wgt_ashed_g",
            "wgt_dried_g",
            "wgt_prefilt_g"
        ],
        "{out}"
    );
    assert_eq!(
        names(&out, "outputs"),
        vec!["AFDM_mgL", "TSS_dry_weight_mgL"],
        "{out}"
    );
    assert_eq!(out["dynamic_outputs"]["any"], false, "{out}");
    let prelude_used = names(&out, "script_functions_used");
    assert!(
        prelude_used.contains(&"calcTSS".to_string()),
        "the prelude functions the entry calls: {prelude_used:?}"
    );

    let (script, entry) = active_script(&db, "chlorophyll").await;
    let (status, out) = inspect(
        &app,
        &json!({ "script": script, "entry_function": entry }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(
        names(&out, "curves"),
        vec!["chla_acid", "chla_noacid"],
        "the curve slots the script resolves: {out}"
    );
}

/// Expected behaviour: a script that builds its output keys at run time reports that, so the
/// detected list is read as a floor rather than the whole set.
#[tokio::test]
#[serial]
async fn runtime_built_output_names_are_reported_as_dynamic() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "runtime_built_output_names_are_reported_as_dynamic",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_inspect_dynamic").await {
        return;
    }
    let (db, app, admin) = setup().await;
    let (script, entry) = active_script(&db, "pco2").await;

    let (status, out) = inspect(
        &app,
        &json!({ "script": script, "entry_function": entry }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["dynamic_outputs"]["any"], true, "{out}");
    let expressions = names(&out["dynamic_outputs"], "expressions");
    assert!(
        expressions.iter().any(|e| e.contains("paste0")),
        "the expressions responsible are named: {expressions:?}"
    );
    assert!(
        names(&out, "outputs").len() < 6,
        "the per-replicate keys cannot be detected, so outputs is a floor: {out}"
    );
    assert!(
        names(&out, "constants").contains(&"gas_const_r_mol".to_string()),
        "{out}"
    );
}

#[tokio::test]
#[serial]
async fn a_script_that_does_not_parse_is_an_answer_not_an_error() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_script_that_does_not_parse_is_an_answer_not_an_error",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_inspect_broken").await {
        return;
    }
    let (_db, app, admin) = setup().await;

    let (status, out) = inspect(
        &app,
        &json!({ "script": "tool <- function(inputs, constants, curves) {\n  x <- 1 +\n}\n" }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "a half-written script is inspectable: {out}");
    assert_eq!(out["parse_ok"], false, "{out}");
    assert_eq!(out["parse_error"]["line"], 3, "{out}");
    assert!(
        out["parse_error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unexpected"),
        "{out}"
    );
    assert_eq!(
        out["inputs"],
        json!([]),
        "the report keeps its shape: {out}"
    );
    assert_eq!(out["entry_found"], false, "{out}");
}

#[tokio::test]
#[serial]
async fn a_manifest_is_reconciled_in_both_directions() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_manifest_is_reconciled_in_both_directions",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_inspect_reconcile").await {
        return;
    }
    let (db, app, admin) = setup().await;
    let (script, entry) = active_script(&db, "tss_afdm").await;

    let manifest = json!({
        "label": "Partial TSS",
        "params": [
            { "name": "wgt_dried_g", "label": "Dried", "kind": "number" },
            { "name": "wgt_prefilt_g", "label": "Pre-filter", "kind": "number" },
            { "name": "salinity_psu", "label": "Salinity", "kind": "number" }
        ],
        "curves": [ { "name": "std_curve", "label": "Standard curve" } ],
    });
    let (status, out) = inspect(
        &app,
        &json!({ "script": script, "entry_function": entry, "manifest": manifest }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{out}");

    let reconciliation = &out["reconciliation"];
    assert_eq!(
        names(reconciliation, "undeclared_inputs"),
        vec!["vol_filtered_ml", "wgt_ashed_g"],
        "read by the script, absent from the manifest: {out}"
    );
    assert_eq!(
        names(reconciliation, "unread_params"),
        vec!["salinity_psu"],
        "declared but never read: {out}"
    );
    assert_eq!(
        names(reconciliation, "unread_curves"),
        vec!["std_curve"],
        "{out}"
    );
    assert_eq!(reconciliation["undeclared_curves"], json!([]), "{out}");
    assert_eq!(
        reconciliation["reads_complete"], true,
        "this script names every read: {out}"
    );
    assert_eq!(reconciliation["outputs_complete"], true, "{out}");

    let (_, without) = inspect(
        &app,
        &json!({ "script": "tool <- function(inputs) list(x = inputs$a)" }),
        &admin,
    )
    .await;
    assert_eq!(
        without["reconciliation"],
        json!(null),
        "no manifest, no comparison: {without}"
    );
}

/// A manifest whose declared outputs exceed what the parse tree can show is not thereby wrong,
/// and the flag saying so travels with the comparison.
#[tokio::test]
#[serial]
async fn reconciling_a_dynamic_script_reports_its_lists_as_incomplete() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "reconciling_a_dynamic_script_reports_its_lists_as_incomplete",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_inspect_reconcile_dynamic").await {
        return;
    }
    let (db, app, admin) = setup().await;
    let (script, entry) = active_script(&db, "chlorophyll").await;

    let (status, out) = inspect(
        &app,
        &json!({ "script": script, "entry_function": entry, "manifest": json!({
            "label": "Chlorophyll",
            "params": [ { "name": "fluor_before", "label": "Fluorescence", "kind": "number" } ],
            "curves": [ { "name": "chla_acid", "label": "Chla acid" } ],
        }) }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(
        out["reconciliation"]["reads_complete"], false,
        "the script reads names it builds at run time: {out}"
    );
    assert_eq!(out["reconciliation"]["outputs_complete"], false, "{out}");
    assert_eq!(
        names(&out["reconciliation"], "undeclared_curves"),
        vec!["chla_noacid"],
        "{out}"
    );
}

#[tokio::test]
#[serial]
async fn no_api_token_reaches_the_inspector() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db);

    let (status, out) = inspect(
        &app,
        &json!({ "script": "tool <- function(inputs) list(x = inputs$a)" }),
        &token,
    )
    .await;
    assert!(
        status == 401 || status == 403,
        "a full-permission token is still not an Administrator ({status}): {out}"
    );
}
