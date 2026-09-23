//! The tool handlers: the catalog, a calculation's closure, and script authoring,
//! versioning, validation and activation.

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router, middleware};
use crudcrate::CRUDResource;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
    TransactionTrait,
};
use std::collections::HashMap;
use uuid::Uuid;

use super::flows::{calculation_sites, execute_and_store_run, preview_run, replay_trace};
use super::models::activation as activation_entity;
use super::models::script as script_entity;
use super::models::script::{ToolScript, ToolScriptList};
use super::models::version::ToolScriptVersion;
use super::models::{
    ActivateRequest, ActivateResponse, ActivationRecord, ActiveTool, CalculationHealth,
    CalculationSites, ClosureQuery, ClosureResponse, CreateScriptRequest, CreateVersionRequest,
    CreateVersionResponse, DraftRunFailure, DraftRunFailureKind, DraftRunRequest, DraftRunResponse,
    DraftRunResults, Engine, FormulaDraftRunRequest, FormulaDraftRunResponse,
    FormulaDraftRunResults, InspectScriptRequest, InspectScriptResponse, LintFinding,
    MissingConstant, RunTrace, SaveFormulaSetRequest, SaveFormulaSetResponse, SavedFormula,
    ToolCalculation, ToolDescriptor, ToolResult, UpdateScriptRequest, ValidateResponse,
    VersionLedgerRow, VersionUsage, parse_manifest, reconcile_manifest, run as tool_run,
};
use super::service::{
    FormulaWrite, LIST_LIMIT, audit_after_activation, calculation_health, calculation_slots,
    calculations_fed_by_subject, canonical_hash, check_engine, check_manifest_against_catalog,
    check_manifest_codes_resolve, closure_subject, codes_held_elsewhere, coverage_for,
    find_active_tool, formula_codes_held_elsewhere, insert_version, lint_script, list_active_tools,
    load_parameter_catalog, load_script, load_version, manifest_finding, manifest_json,
    mint_formula_version, normalise_name, normalised_json, plan_formula_set, render,
    replicated_for, run_stored_cases, run_tool_body, runner_runtime, stored_version_content,
    take_back_steps,
};
use crate::common::AppState;
use crate::common::middleware::{AuthContext, ProjectScope, scope_site_ids};
use crate::error::{AppError, AppResult};
use crate::routes::private::derived_parameters::models::definition as formula_entity;
use crate::routes::private::derived_parameters::models::definition::{
    CalculationFormula, CalculationFormulaCreate, CalculationFormulaUpdate,
};

/// List the active analytical tools with their full input/output manifests.
///
/// Each output carries its declaration plus `parameter`, the catalog row it resolves to here:
/// `parameter_id` first, then `suggested_parameter_code` case-insensitively, null when neither
/// names a row this database holds. `dangling_parameter_id` on the resolved parameter says the id
/// named a row that has since gone and the code carried the output instead. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/api/tools",
    responses(
        (status = 200, description = "List of tool descriptors", body = [ToolDescriptor]),
    ),
    tag = "tools"
)]
pub async fn list_tools(State(state): State<AppState>) -> AppResult<Json<Vec<ToolDescriptor>>> {
    let tools = list_active_tools(&state.db).await?;
    let catalog =
        load_parameter_catalog(&state.db, tools.iter().map(|tool| &tool.manifest)).await?;
    Ok(Json(
        tools.iter().map(|tool| tool.descriptor(&catalog)).collect(),
    ))
}

/// Run an analytical tool calculation. The body schema is the tool's manifest (call `GET /tools`);
/// unknown fields are refused by name. Requires `read_data`.
#[utoipa::path(
    post,
    path = "/api/tools/{tool_name}/calculate",
    params(("tool_name" = String, Path, description = "Tool name (e.g. 'doc', 'dic', 'pco2')")),
    request_body(content = Object, description = "Per-tool request body (see GET /tools for schemas)"),
    responses(
        (status = 200, description = "Calculation result with `inputs_used` / `inputs_ignored` accounting", body = ToolResult),
        (status = 404, description = "Unknown tool name"),
        (status = 409, description = "The calculation is switched off"),
        (status = 400, description = "Invalid input for this tool, or a script error"),
        (status = 503, description = "The tool runner is not configured or unreachable"),
    ),
    tag = "tools"
)]
pub async fn calculate_tool(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(tool_name): Path<String>,
    body: axum::body::Bytes,
) -> AppResult<Json<ToolResult>> {
    let tool = find_active_tool(&state.db, &tool_name).await?;
    let result = execute_and_store_run(
        &state,
        &tool,
        &body,
        &crate::common::actor::label(&auth),
        "interactive",
    )
    .await?;
    Ok(Json(result))
}

/// Run a tool calculation without storing it, for a form computing as values are typed. The
/// response has no `run_id`, so a save cannot name it; `calculate` stores the run a save names.
/// Requires `read_data`.
#[utoipa::path(
    post,
    path = "/api/tools/{tool_name}/preview",
    params(("tool_name" = String, Path, description = "Tool name (e.g. 'doc', 'dic', 'pco2')")),
    request_body(content = Object, description = "Per-tool request body (see GET /tools for schemas)"),
    responses(
        (status = 200, description = "The calculation, stored nowhere", body = ToolCalculation),
        (status = 404, description = "Unknown tool name"),
        (status = 409, description = "The calculation is switched off"),
        (status = 400, description = "Invalid input for this tool, or a script error"),
        (status = 503, description = "The tool runner is not configured or unreachable"),
    ),
    tag = "tools"
)]
pub async fn preview_tool(
    State(state): State<AppState>,
    Path(tool_name): Path<String>,
    body: axum::body::Bytes,
) -> AppResult<Json<ToolCalculation>> {
    let tool = find_active_tool(&state.db, &tool_name).await?;
    Ok(Json(preview_run(&state, &tool, &body).await?))
}

/// Replay a stored run under the version it pinned, so a computed value shows its formula, its
/// intermediates and the values each of them read. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/api/tool_runs/{id}/trace",
    params(("id" = Uuid, Path, description = "Tool run id")),
    responses(
        (status = 200, description = "Each formula as the run evaluated it", body = RunTrace),
        (status = 404, description = "No such run"),
        (status = 409, description = "A script run, or a version that is no longer stored"),
    ),
    tag = "tools"
)]
pub async fn trace_run(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<RunTrace>> {
    let run = tool_run::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Tool run {id} not found")))?;
    Ok(Json(replay_trace(&state.db, &run).await?))
}

/// The calculations a set of parameters feeds, and where each calculation's data lives.
#[utoipa::path(
    get,
    path = "/api/calculations/closure",
    params(ClosureQuery),
    responses(
        (status = 200, description = "Calculations fed, with optional slot coverage", body = ClosureResponse),
        (status = 400, description = "Invalid query parameters"),
    ),
    tag = "tools"
)]
pub async fn get_calculation_closure(
    State(state): State<AppState>,
    Query(query): Query<ClosureQuery>,
) -> AppResult<Response> {
    let subject = closure_subject(&query)?;
    let calculations = calculations_fed_by_subject(&state.db, &subject).await?;

    // Coverage covers every slot an active calculation touches, not only the ones asked about: an
    // admin's question is "is this calculation wired up anywhere", which the named set cannot answer.
    let coverage = if query.include_coverage {
        let slots = calculation_slots(&state).await?;
        coverage_for(&state, &slots, query.site_id).await?
    } else {
        Vec::new()
    };

    // A constant is asked about before it is edited, so the answer says what has already been
    // computed from it, not only what would read it today.
    let stored = match &subject {
        crate::routes::private::tools::models::Subject::Constant(id) => {
            crate::routes::private::tools::service::stored_usage_of_constant(&state.db, *id).await?
        }
        _ => None,
    };

    Ok(Json(ClosureResponse {
        calculations,
        coverage,
        stored,
    })
    .into_response())
}

/// Every enabled calculation and the sites it is active at, confined to the caller's projects.
#[utoipa::path(
    get,
    path = "/api/calculations/sites",
    responses(
        (status = 200, description = "Where each calculation fires", body = [CalculationSites]),
    ),
    tag = "tools"
)]
pub async fn get_calculation_sites(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<Vec<CalculationSites>>> {
    let site_ids = scope_site_ids(&state.db, &scope).await?;
    Ok(Json(
        calculation_sites(&state.db, site_ids.as_deref()).await?,
    ))
}

/// The open event-audit findings each calculation is carrying, and how many visits they sit on.
#[utoipa::path(
    get,
    path = "/api/calculations/health",
    responses(
        (status = 200, description = "Open findings per calculation", body = [CalculationHealth]),
    ),
    tag = "tools"
)]
pub async fn get_calculation_health(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<CalculationHealth>>> {
    Ok(Json(calculation_health(&state.db).await?))
}

/// List every calculation with its live version and how many versions it has. Requires
/// Administrator.
#[utoipa::path(get, path = "/api/tool_scripts",
    responses((status = 200, body = [ToolScriptList])), tag = "tool_scripts")]
pub async fn list_scripts(State(state): State<AppState>) -> AppResult<Json<Vec<ToolScriptList>>> {
    <ToolScript as CRUDResource>::get_all(
        &state.db,
        &sea_orm::Condition::all(),
        super::models::script::Column::Name,
        sea_orm::Order::Asc,
        0,
        LIST_LIMIT,
    )
    .await
    .map(Json)
    .map_err(|e| AppError::Internal(e.to_string()))
}

/// One calculation with its version history, newest first. Requires Administrator.
#[utoipa::path(get, path = "/api/tool_scripts/{id}", params(("id" = Uuid, Path)),
    responses((status = 200, body = ToolScript)), tag = "tool_scripts")]
pub async fn get_script(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<ToolScript>> {
    Ok(Json(load_script(&state, id).await?))
}

/// Create a calculation (no versions yet; it lists in `GET /tools` only once a version is
/// activated). `created_by` is the authenticated caller. Requires Administrator.
#[utoipa::path(post, path = "/api/tool_scripts", request_body = CreateScriptRequest,
    responses((status = 200, body = ToolScript)), tag = "tool_scripts")]
pub async fn create_script(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(payload): Json<CreateScriptRequest>,
) -> AppResult<Json<ToolScript>> {
    let name = normalise_name(&payload.name).map_err(|e| AppError::BadRequest(e.to_string()))?;
    if let Some(engine) = payload.engine.as_deref() {
        check_engine(engine).map_err(|e| AppError::BadRequest(e.to_string()))?;
    }
    let model = super::models::script::ActiveModel {
        id: Set(Uuid::new_v4()),
        name: Set(name.clone()),
        label: Set(payload.label),
        description: Set(payload.description),
        // Never from the request: a self-asserted author is not a trail.
        created_by: Set(Some(crate::common::actor::label(&auth))),
        engine: Set(payload.engine.unwrap_or_else(|| "script".to_string())),
        ..Default::default()
    };
    let created = model.insert(&state.db).await.map_err(|e| {
        let message = e.to_string();
        if message.contains("idx_tool_scripts_name") {
            AppError::Conflict(format!("a tool named '{name}' already exists"))
        } else {
            AppError::Database(e)
        }
    })?;
    Ok(Json(load_script(&state, created.id).await?))
}

/// Update a calculation's label, description or enabled switch (the code lives in versions).
/// Requires Administrator.
#[utoipa::path(patch, path = "/api/tool_scripts/{id}", params(("id" = Uuid, Path)),
    request_body = UpdateScriptRequest,
    responses((status = 200, body = ToolScript)), tag = "tool_scripts")]
pub async fn update_script(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateScriptRequest>,
) -> AppResult<Json<ToolScript>> {
    let existing = super::models::script::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("tool script {id} not found")))?;
    let mut model: super::models::script::ActiveModel = existing.into();
    if let Some(label) = payload.label {
        model.label = Set(label);
    }
    if payload.description.is_some() {
        model.description = Set(payload.description);
    }
    if let Some(enabled) = payload.enabled {
        model.enabled = Set(enabled);
    }
    model.updated_at = Set(chrono::Utc::now());
    model.update(&state.db).await?;
    Ok(Json(load_script(&state, id).await?))
}

/// Append an immutable version. Refused when the lint finds forbidden constructs or a syntax
/// error, the manifest does not parse, an output names a `parameter_id` or the manifest names a
/// constant that does not exist, an output's `parameter_id` and `suggested_parameter_code` name
/// different parameters, two outputs resolve to one parameter, or an identical version already
/// exists. An output whose `suggested_parameter_code` matches no parameter is stored and reported
/// in `lint`: an author may declare an analyte before a manager creates it.
///
/// The manifest that is stored is the one an author sent plus the code of every output that named
/// only an id, so the stored version carries the half that survives leaving this database. A
/// version is identified by its whole content, and that content is the stamped manifest, so a
/// manifest-only or case-only edit is a new version. `created_by` is the authenticated caller.
/// Requires Administrator.
#[utoipa::path(post, path = "/api/tool_scripts/{id}/versions", params(("id" = Uuid, Path)),
    request_body = CreateVersionRequest,
    responses((status = 200, body = CreateVersionResponse),
              (status = 400, description = "Invalid manifest"),
              (status = 409, description = "Script lint or manifest findings, listed in detail"),
              (status = 503, description = "The tool runner is not configured or unreachable")),
    tag = "tool_scripts")]
pub async fn create_version(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(payload): Json<CreateVersionRequest>,
) -> AppResult<Json<CreateVersionResponse>> {
    load_script(&state, id).await?;

    // The manifest is refused by rules this API owns and the lint is read off the runner, so the
    // manifest is settled first: a misspelled kind names its field whether or not the sidecar is
    // up, instead of the outage answering for it.
    //
    // The stored manifest is this value, not the payload: an output naming only an id has the
    // resolved code written into it here, so the version carries the half that travels.
    let mut stored_manifest = payload.manifest.clone();
    let mut manifest = parse_manifest(&stored_manifest)
        .map_err(|e| AppError::BadRequest(format!("invalid manifest: {e}")))?;
    let catalog = check_manifest_against_catalog(
        &state.db,
        &mut manifest,
        Some(&mut stored_manifest),
        MissingConstant::Refuse,
    )
    .await?;
    if !catalog.errors.is_empty() {
        let findings: Vec<LintFinding> = catalog.errors.into_iter().map(manifest_finding).collect();
        return Err(AppError::ConflictDetail {
            message: "the manifest names catalog entries that do not exist".to_string(),
            detail: serde_json::to_value(&findings).unwrap_or_default(),
        });
    }
    let warnings: Vec<LintFinding> = catalog.warnings.into_iter().map(manifest_finding).collect();

    let findings = lint_script(&state, &payload.script).await?;
    if !findings.is_empty() {
        return Err(AppError::ConflictDetail {
            message: "the script did not pass the safety lint".to_string(),
            detail: serde_json::to_value(&findings).unwrap_or_default(),
        });
    }

    let entry = payload.entry_function.unwrap_or_else(|| "tool".to_string());
    let test_cases = payload.test_cases.unwrap_or_else(|| serde_json::json!({}));
    // Hashed and stored in the form jsonb holds, so the hash on the row is recomputable from the
    // row: a fetched version re-posted unchanged is recognised as the duplicate it is.
    let stored = stored_version_content(
        &state.db,
        &payload.script,
        &entry,
        &stored_manifest,
        &test_cases,
    )
    .await?;
    let txn = state.db.begin().await?;
    let vid = insert_version(
        &txn,
        super::models::version::ActiveModel {
            tool_script_id: Set(id),
            script: Set(payload.script),
            entry_function: Set(entry),
            manifest: Set(normalised_json(&stored.manifest)?),
            test_cases: Set(normalised_json(&stored.test_cases)?),
            content_hash: Set(stored.content_hash),
            note: Set(payload.note),
            created_by: Set(Some(crate::common::actor::label(&auth))),
            ..Default::default()
        },
    )
    .await?;
    txn.commit().await?;

    let mut version = load_version(&state, id, vid).await?;
    version.active = false;
    Ok(Json(CreateVersionResponse {
        version,
        lint: warnings,
    }))
}

/// Classify an error from the run into the account a draft reports. Anything that is not one of
/// the three run outcomes (a database failure, say) is not about the draft and stays an error.
pub(super) fn draft_failure(error: AppError) -> Result<DraftRunFailure, AppError> {
    let plain = |kind, message| DraftRunFailure {
        kind,
        message,
        call: None,
        traceback: Vec::new(),
    };
    Ok(match error {
        AppError::ToolScriptError {
            message,
            call,
            traceback,
        } => DraftRunFailure {
            kind: DraftRunFailureKind::ScriptError,
            message,
            call,
            traceback,
        },
        AppError::ServiceUnavailable(message) => {
            plain(DraftRunFailureKind::RunnerUnavailable, message)
        }
        // How the runner reports a condition raised before the tool was entered: R text rather
        // than the structured tool error, but a script failure all the same.
        AppError::BadRequest(message) if message.starts_with("tool script error:") => {
            plain(DraftRunFailureKind::ScriptError, message)
        }
        AppError::BadRequest(message) => plain(DraftRunFailureKind::BodyRefused, message),
        other => return Err(other),
    })
}

/// Run unsaved editor content: script, entry function, manifest and a request body, through the
/// same manifest validation, constant resolution and curve resolution as
/// `POST /tools/{name}/calculate`, so a draft that runs green is a tool that works. Writes
/// nothing. Requires Administrator.
///
/// A run that ends without results answers 200 with `ran: false` and a `failure`, so the lint
/// findings computed before it are reported rather than replaced by the first thing that went
/// wrong. `POST /tools/{name}/calculate` keeps the opposite behaviour: a calculation that fails
/// is a failed request. Only content that could not be read at all is a 400 here, because there
/// is nothing to report findings about: a request body that is not the expected JSON, or a
/// manifest that does not parse (named by the path that was refused).
#[utoipa::path(post, path = "/api/tool_scripts/draft_run", request_body = DraftRunRequest,
    responses((status = 200, body = DraftRunResponse,
               description = "The lint findings, plus results or the reason the run ended"),
              (status = 400, description = "Unreadable request body or manifest")),
    tag = "tool_scripts")]
pub async fn draft_run(
    State(state): State<AppState>,
    Json(payload): Json<DraftRunRequest>,
) -> AppResult<Json<DraftRunResponse>> {
    let mut draft_manifest = payload.manifest.clone();
    let mut manifest = parse_manifest(&draft_manifest)
        .map_err(|e| AppError::BadRequest(format!("invalid manifest: {e}")))?;
    let entry = payload
        .entry_function
        .clone()
        .unwrap_or_else(|| "tool".to_string());
    // The same lint the save path runs. A runner that cannot lint is reported by the run that
    // follows, which reaches the same runner, so the findings are dropped rather than turned into
    // a second account of one outage. The manifest's catalog references are checked here too, so
    // an author sees what the save path would refuse without having to attempt the save. The
    // resolved codes are stamped as the save would stamp them, before the hash, so a draft and
    // the version it becomes carry one identity.
    let mut lint = lint_script(&state, &payload.script)
        .await
        .unwrap_or_default();
    let catalog = check_manifest_against_catalog(
        &state.db,
        &mut manifest,
        Some(&mut draft_manifest),
        MissingConstant::Omit,
    )
    .await?;
    lint.extend(
        catalog
            .errors
            .into_iter()
            .chain(catalog.warnings)
            .map(manifest_finding),
    );
    // A draft has no stored cases, so its content identity covers the three parts it does have.
    // Normalised through jsonb like the save path, or a draft would carry a different identity
    // from the version it becomes for any manifest jsonb re-renders.
    let content_hash = stored_version_content(
        &state.db,
        &payload.script,
        &entry,
        &draft_manifest,
        &serde_json::json!({}),
    )
    .await?
    .content_hash;

    let tool = ActiveTool::draft(payload.script, entry, manifest, content_hash);
    let body = serde_json::to_vec(&payload.inputs.unwrap_or_else(|| serde_json::json!({})))
        .unwrap_or_default();
    let outcome = run_tool_body(
        &state,
        &tool,
        &body,
        payload.constants.as_ref(),
        MissingConstant::Omit,
    )
    .await;
    let (run, failure) = match outcome {
        Ok(outcome) => (
            Some(DraftRunResults {
                results: serde_json::Value::Object(outcome.results),
                inputs_used: outcome.inputs_used,
                inputs_ignored: outcome.inputs_ignored,
                constants: serde_json::Value::Object(outcome.constants),
                curves: outcome.curves,
            }),
            None,
        ),
        Err(e) => (None, Some(draft_failure(e)?)),
    };
    let runtime = runner_runtime(&state).await;
    Ok(Json(DraftRunResponse {
        ran: run.is_some(),
        run,
        failure,
        tool_version: tool.version_ref(runtime.as_ref()),
        lint,
    }))
}

/// Run an unsaved formula set at a visit, in place of the calculation's stored formulas.
///
/// Each formula's variables resolve as a save would resolve them (a catalog parameter, a
/// constant, a column of `sites`), the set is ordered and checked as a version mint would check
/// it, and the run reads the visit exactly as `/tools/{name}/calculate` does. Nothing is stored:
/// no version, no run row, no reading. A formula the set refuses (an unknown variable, a cycle,
/// an unreadable expression) is a 400 naming it; a run that ends without results reports why at
/// 200, as the script draft run does.
#[utoipa::path(post, path = "/api/tool_scripts/{id}/formulas/draft_run",
    params(("id" = Uuid, Path, description = "The formula calculation")),
    request_body = FormulaDraftRunRequest,
    responses((status = 200, body = FormulaDraftRunResponse,
               description = "The results or the reason the run ended, and the manifest the set implies"),
              (status = 400, description = "A formula the set refuses, or a calculation that is not formula-engined"),
              (status = 404, description = "No such calculation")),
    tag = "tool_scripts")]
pub async fn draft_run_formulas(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<FormulaDraftRunRequest>,
) -> AppResult<Json<FormulaDraftRunResponse>> {
    let script = load_script(&state, id).await?;
    if Engine::parse(&script.engine) != Some(Engine::Formula) {
        return Err(AppError::BadRequest(format!(
            "{} is a {} calculation; a formula draft runs on a formula calculation",
            script.name, script.engine
        )));
    }
    let formulas = super::service::pin_draft_formulas(&state.db, &payload.formulas).await?;
    let replicated = replicated_for(&state.db).await?;
    let manifest_value = manifest_json(
        &script.label,
        script.description.as_deref(),
        &formulas,
        &replicated,
    )
    .map_err(AppError::BadRequest)?;
    let manifest = parse_manifest(&manifest_value)
        .map_err(|e| AppError::BadRequest(format!("invalid manifest: {e}")))?;
    let body = render(&formulas).map_err(AppError::BadRequest)?;
    let content_hash = canonical_hash(&serde_json::json!({
        "script": body,
        "manifest": manifest_value,
    }));
    let tool = ActiveTool::draft_formulas(&script, manifest, content_hash, formulas);
    let inputs = serde_json::to_vec(&payload.inputs.unwrap_or_else(|| serde_json::json!({})))
        .unwrap_or_default();
    let outcome = run_tool_body(
        &state,
        &tool,
        &inputs,
        payload.constants.as_ref(),
        MissingConstant::Omit,
    )
    .await;
    let (run, failure) = match outcome {
        Ok(outcome) => (
            Some(FormulaDraftRunResults {
                results: serde_json::Value::Object(outcome.results),
                skipped: outcome.skipped,
                inputs_used: outcome.inputs_used,
                inputs_ignored: outcome.inputs_ignored,
                constants: serde_json::Value::Object(outcome.constants),
                curves: outcome.curves,
                site_inputs: outcome.site_inputs,
                event_inputs: outcome.event_inputs,
                trace: outcome.trace,
            }),
            None,
        ),
        Err(e) => (None, Some(draft_failure(e)?)),
    };
    Ok(Json(FormulaDraftRunResponse {
        ran: run.is_some(),
        run,
        failure,
        manifest: manifest_value,
    }))
}

/// The message a refused formula carries, without the status prefix the error type adds.
pub(super) fn api_message(e: crudcrate::ApiError) -> String {
    match e {
        crudcrate::ApiError::BadRequest { message } => message,
        other => AppError::from(other).to_string(),
    }
}

/// Read what a script declares, reads and returns, without running it.
///
/// The script is parsed in the runner and its tree walked, so an unparseable or hostile script is
/// inspected as safely as it is read: a syntax error comes back as `parse_ok: false` with the
/// line, at status 200, because a script being typed is unparseable most of the time.
///
/// **`outputs` is a floor, not a complete list.** Keys assembled at run time (`paste0` with a
/// replicate letter, as pco2 and nutrients do) do not exist in the source, so they cannot be
/// detected. `dynamic_outputs.any` is true exactly when that happened, and while it is true a
/// caller must not treat `outputs` as the whole set, nor a manifest declaring more outputs than
/// were detected as wrong. `dynamic_reads.any` says the same about `inputs`/`constants`/`curves`.
///
/// With a `manifest` in the request the response also carries `reconciliation`: which detected
/// names the manifest does not declare, and which declared names the script does not read,
/// qualified by those two completeness flags. It is a comparison only, it generates no manifest.
/// Requires Administrator.
#[utoipa::path(post, path = "/api/tool_scripts/inspect", request_body = InspectScriptRequest,
    responses((status = 200, body = InspectScriptResponse),
              (status = 400, description = "Invalid manifest"),
              (status = 503, description = "The tool runner is not configured or unreachable")),
    tag = "tool_scripts")]
pub async fn inspect_script(
    State(state): State<AppState>,
    Json(payload): Json<InspectScriptRequest>,
) -> AppResult<Json<InspectScriptResponse>> {
    let manifest = payload
        .manifest
        .map(|raw| {
            parse_manifest(&raw).map_err(|e| AppError::BadRequest(format!("invalid manifest: {e}")))
        })
        .transpose()?;
    let entry = payload.entry_function.as_deref().unwrap_or("tool");
    let inspection = super::service::inspect_script(&state, &payload.script, entry).await?;
    let reconciliation = manifest
        .as_ref()
        .map(|m| reconcile_manifest(&inspection, m));
    Ok(Json(InspectScriptResponse {
        inspection,
        reconciliation,
    }))
}

/// Full version content (script text, manifest, cases). Requires Administrator.
#[utoipa::path(get, path = "/api/tool_scripts/{id}/versions/{version_id}",
    params(("id" = Uuid, Path), ("version_id" = Uuid, Path)),
    responses((status = 200, body = ToolScriptVersion)), tag = "tool_scripts")]
pub async fn get_version(
    State(state): State<AppState>,
    Path((id, vid)): Path<(Uuid, Uuid)>,
) -> AppResult<Json<ToolScriptVersion>> {
    Ok(Json(load_version(&state, id, vid).await?))
}

/// Run a version's stored test cases through the runner. All-pass stamps `validated_at`; a
/// failure clears it. Requires Administrator.
#[utoipa::path(post, path = "/api/tool_scripts/{id}/versions/{version_id}/validate",
    params(("id" = Uuid, Path), ("version_id" = Uuid, Path)),
    responses((status = 200, body = ValidateResponse)), tag = "tool_scripts")]
pub async fn validate_version(
    State(state): State<AppState>,
    Path((id, vid)): Path<(Uuid, Uuid)>,
) -> AppResult<Json<ValidateResponse>> {
    let version = load_version(&state, id, vid).await?;
    Ok(Json(run_stored_cases(&state, id, &version).await?))
}

/// Make a version the one `GET /tools` serves. Activating an older version is the rollback; every
/// flip lands in the activation audit under the authenticated caller.
///
/// A version has to have been validated by hand, **and its cases are run again here** rather than
/// read off `validated_at`. The stamp says the cases passed at a time, and what a case runs
/// against outlives it: the constants table and the standard curves a case resolves are shared,
/// editable state, so a version validated in March can be wrong by June without anything about
/// the version changing. Activation is the moment it starts answering
/// `POST /tools/{name}/calculate`, which is the moment worth spending a case run on. The stamp
/// still gates the workflow, so an author cannot skip seeing the cases pass; it is no longer the
/// only thing standing between a failing version and production.
///
/// The price is that activation needs the runner: with the sidecar down there is no rollback, and
/// no tool is calculating anything either way.
///
/// The manifest is re-checked against the catalog and the findings come back in `lint`. Those do
/// not block: a version whose outputs still resolve serves correctly, and refusing to activate it
/// would leave the operator with no way to put the repaired version live either. Requires
/// Administrator.
#[utoipa::path(post, path = "/api/tool_scripts/{id}/versions/{version_id}/activate",
    params(("id" = Uuid, Path), ("version_id" = Uuid, Path)),
    request_body = ActivateRequest,
    responses((status = 200, body = ActivateResponse),
              (status = 409, description = "Never validated, or the cases do not pass now"),
              (status = 503, description = "The tool runner is not configured or unreachable")),
    tag = "tool_scripts")]
pub async fn activate_version(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path((id, vid)): Path<(Uuid, Uuid)>,
    Json(payload): Json<ActivateRequest>,
) -> AppResult<Json<ActivateResponse>> {
    let version = load_version(&state, id, vid).await?;
    if version.validated_at.is_none() {
        return Err(AppError::Conflict(
            "this version has not passed validation; run validate first".to_string(),
        ));
    }
    let validation = run_stored_cases(&state, id, &version).await?;
    if !validation.passed {
        return Err(AppError::ConflictDetail {
            message: "this version's test cases do not pass; it cannot be activated".to_string(),
            detail: serde_json::to_value(&validation.cases).unwrap_or_default(),
        });
    }
    let mut manifest = parse_manifest(&version.manifest)
        .map_err(|e| AppError::BadRequest(format!("invalid manifest: {e}")))?;
    let summary = load_script(&state, id).await?;
    // Every parameter a manifest names has to be one the catalog holds. Checked at activation,
    // where the version becomes the one that runs. Which group those parameters belong to decides
    // nothing (Q135).
    check_manifest_codes_resolve(&state.db, &summary.name, &version.manifest).await?;
    let catalog =
        check_manifest_against_catalog(&state.db, &mut manifest, None, MissingConstant::Refuse)
            .await?;
    let lint: Vec<LintFinding> = catalog
        .errors
        .into_iter()
        .chain(catalog.warnings)
        .map(manifest_finding)
        .collect();
    let txn = state.db.begin().await?;
    // The row is locked for the read: the version being replaced is what the activation records,
    // so a concurrent activation must not slip between reading it and writing the new one.
    let current = script_entity::Entity::find_by_id(id)
        .lock_exclusive()
        .one(&txn)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Tool script {id} not found")))?;
    activation_entity::ActiveModel {
        tool_script_id: Set(id),
        from_version_id: Set(current.active_version_id),
        to_version_id: Set(vid),
        activated_by: Set(Some(crate::common::actor::label(&auth))),
        ..Default::default()
    }
    .insert(&txn)
    .await?;
    script_entity::Entity::update_many()
        .col_expr(
            script_entity::Column::ActiveVersionId,
            Expr::value(Some(vid)),
        )
        .col_expr(script_entity::Column::UpdatedAt, Expr::current_timestamp())
        .filter(script_entity::Column::Id.eq(id))
        .exec(&txn)
        .await?;
    txn.commit().await?;
    let script = load_script(&state, id).await?;
    // The audit is the backstop under either arm: it reports what the activation left disagreeing.
    audit_after_activation(&state.db, &script.name).await;
    // The correcting arm reaches exactly the visits the superseded version produced values at
    // (Q170). It is enqueued after the commit, so a failure here leaves the activation standing
    // and the audit's findings say what was not repaired.
    super::service::recompute_after_activation(
        &state.db,
        payload.migrate_stored,
        &script.name,
        id,
        current.active_version_id,
    )
    .await;
    Ok(Json(ActivateResponse { script, lint }))
}

/// A saved formula as a create: the calculation is the path's, and an omitted name or unit takes
/// the code, which is what the calculation page sends when an author leaves the field blank.
fn create_model(script_id: Uuid, f: &SavedFormula) -> CalculationFormulaCreate {
    CalculationFormulaCreate {
        code: f.code.clone(),
        name: f.name.clone().unwrap_or_else(|| f.code.clone()),
        units: f.units.clone().unwrap_or_default(),
        formula: f.formula.clone(),
        description: f.description.clone(),
        tool_script_id: Some(script_id),
        ordinal: Some(f.ordinal),
        curve_slot: f.curve_slot.clone(),
        per_replicate: f.per_replicate.clone(),
        intermediate: Some(f.intermediate),
    }
}

/// A saved formula as an update. The set is the whole truth, so every field is written, including
/// the ones the author cleared: a curve slot removed from the form is removed from the row.
fn update_model(f: &SavedFormula) -> CalculationFormulaUpdate {
    CalculationFormulaUpdate {
        code: Some(Some(f.code.clone())),
        name: Some(Some(f.name.clone().unwrap_or_else(|| f.code.clone()))),
        units: Some(Some(f.units.clone().unwrap_or_default())),
        formula: Some(Some(f.formula.clone())),
        description: Some(f.description.clone()),
        tool_script_id: None,
        ordinal: Some(Some(f.ordinal)),
        curve_slot: Some(f.curve_slot.clone()),
        per_replicate: Some(f.per_replicate.clone()),
        intermediate: Some(Some(f.intermediate)),
    }
}

/// Save a formula calculation's whole formula set as one version.
///
/// The set is the request: a formula carrying an `id` updates that row, one without an id is
/// created, and a stored formula the set leaves out is deleted. One version is minted from the
/// resulting set and activated, whatever the save touched, so an author's version history reads as
/// their decisions rather than as their keystrokes (Q186). `migrate_stored` chooses what happens to
/// the values the superseded version produced (Q170). Requires Administrator, or a token with
/// `write_metadata`, which is what a formula row is written under.
#[utoipa::path(
    post,
    path = "/api/tool_scripts/{id}/formulas",
    params(("id" = Uuid, Path, description = "Calculation UUID")),
    request_body = SaveFormulaSetRequest,
    responses(
        (status = 200, description = "The set is saved and one version activated", body = SaveFormulaSetResponse),
        (status = 400, description = "A formula the set refuses, a code another calculation holds, or a calculation that is not formula-engined"),
        (status = 404, description = "No such calculation"),
    ),
    tag = "tool_scripts")]
pub async fn save_formula_set(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(payload): Json<SaveFormulaSetRequest>,
) -> AppResult<Json<SaveFormulaSetResponse>> {
    let script = load_script(&state, id).await?;
    if Engine::parse(&script.engine) != Some(Engine::Formula) {
        return Err(AppError::BadRequest(format!(
            "{} is a {} calculation; a formula set saves on a formula calculation",
            script.name, script.engine
        )));
    }

    let actor = crate::common::actor::label(&auth);
    let txn = state.db.begin().await?;
    // The calculation is locked for the save: the version being superseded is what the migration
    // names, so a concurrent save must not slip between reading it and minting the new one.
    let current = script_entity::Entity::find_by_id(id)
        .lock_exclusive()
        .one(&txn)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Tool script {id} not found")))?;
    let superseded = current.active_version_id;

    let payload_ids: Vec<Uuid> = payload.formulas.iter().filter_map(|f| f.id).collect();
    take_back_steps(&txn, id, &script.name, &payload_ids).await?;

    let before = formula_entity::Entity::find()
        .filter(formula_entity::Column::ToolScriptId.eq(id))
        .order_by_asc(formula_entity::Column::Ordinal)
        .order_by_asc(formula_entity::Column::Code)
        .all(&txn)
        .await?;
    // What each formula publishes before the save, so a formula this save turns into a step can be
    // told from one that was already a step and the catalog row it leaves behind can be named.
    let published: Vec<(Uuid, Uuid)> = before
        .iter()
        .filter(|f| !f.intermediate)
        .filter_map(|f| f.output_parameter_id.map(|p| (f.id, p)))
        .collect();
    let stored: Vec<(Uuid, String)> = before.into_iter().map(|f| (f.id, f.code)).collect();
    let named: Vec<(Option<Uuid>, String)> = payload
        .formulas
        .iter()
        .map(|f| (f.id, f.code.clone()))
        .collect();
    let writes = plan_formula_set(&stored, &named).map_err(AppError::BadRequest)?;
    let codes: Vec<String> = named.iter().map(|(_, code)| code.clone()).collect();
    let held = formula_codes_held_elsewhere(&txn, id, &codes).await?;
    codes_held_elsewhere(&held).map_err(AppError::BadRequest)?;

    let mut created = 0usize;
    let mut updated = 0usize;
    let mut deleted = 0usize;
    for write in writes {
        match write {
            FormulaWrite::Delete(formula_id) => {
                CalculationFormula::delete(&txn, formula_id).await?;
                deleted += 1;
            }
            FormulaWrite::Create(i) => {
                CalculationFormula::create(&txn, create_model(id, &payload.formulas[i])).await?;
                created += 1;
            }
            FormulaWrite::Update(formula_id, i) => {
                CalculationFormula::update(&txn, formula_id, update_model(&payload.formulas[i]))
                    .await?;
                updated += 1;
            }
        }
    }

    // One version for the whole save, whatever it touched.
    let version_id = mint_formula_version(&txn, id, Some(&actor)).await?;

    // A formula this save turned into a step stops publishing. Nothing is deleted, so the response
    // says what stays behind under each parameter and who still reads it.
    let mut given_up = Vec::new();
    for (formula_id, parameter_id) in published {
        let Some(formula) = formula_entity::Entity::find_by_id(formula_id)
            .one(&txn)
            .await?
        else {
            continue;
        };
        if !formula.intermediate {
            continue;
        }
        given_up.push(
            crate::routes::private::derived_parameters::service::given_up_report(
                &txn,
                formula.code,
                parameter_id,
            )
            .await?,
        );
    }
    txn.commit().await?;

    let version_no = match version_id {
        Some(vid) => super::models::version::Entity::find_by_id(vid)
            .one(&state.db)
            .await?
            .map(|v| v.version_no),
        None => None,
    };

    // The audit is the backstop under either arm: it reports what the save left disagreeing.
    audit_after_activation(&state.db, &script.name).await;
    let migrated = payload.migrate_stored && superseded.is_some() && version_id != superseded;
    super::service::recompute_after_activation(&state.db, migrated, &script.name, id, superseded)
        .await;

    Ok(Json(SaveFormulaSetResponse {
        version_id,
        version_no,
        created,
        updated,
        deleted,
        migrated,
        given_up,
    }))
}

/// What each version of the calculation has already produced, newest first. Requires
/// Administrator.
///
/// Read before a save or an activation, which offers the author the choice between leaving those
/// values on the version that produced them and recomputing them under the new one.
#[utoipa::path(get, path = "/api/tool_scripts/{id}/version_usage", params(("id" = Uuid, Path)),
    responses((status = 200, body = [VersionUsage])), tag = "tool_scripts")]
pub async fn list_version_usage(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<VersionUsage>>> {
    Ok(Json(super::service::version_usage(&state.db, id).await?))
}

/// What each version of the calculation has computed on the stream arm, newest first (Q232).
///
/// The visit arm answers this with its runs; a stream pass mints none, so the history is read off
/// the curation ledger. Requires `read_data`: it is a reading of what was computed, not of how the
/// calculation is written.
#[utoipa::path(get, path = "/api/tool_scripts/{id}/version_ledger", params(("id" = Uuid, Path)),
    responses((status = 200, body = [VersionLedgerRow])), tag = "tool_scripts")]
pub async fn list_version_ledger(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<VersionLedgerRow>>> {
    Ok(Json(super::service::version_ledger(&state.db, id).await?))
}

/// The script's activation history, newest first. Requires Administrator.
#[utoipa::path(get, path = "/api/tool_scripts/{id}/activations", params(("id" = Uuid, Path)),
    responses((status = 200, body = [ActivationRecord])), tag = "tool_scripts")]
pub async fn list_activations(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<ActivationRecord>>> {
    let rows = super::models::activation::Entity::find()
        .filter(super::models::activation::Column::ToolScriptId.eq(id))
        .order_by_desc(super::models::activation::Column::ActivatedAt)
        .all(&state.db)
        .await?;
    // The audit stores version ids; a reader wants the numbers, and one lookup answers every row.
    let numbers: HashMap<Uuid, i32> = super::models::version::Entity::find()
        .filter(super::models::version::Column::ToolScriptId.eq(id))
        .all(&state.db)
        .await?
        .into_iter()
        .map(|v| (v.id, v.version_no))
        .collect();
    Ok(Json(
        rows.into_iter()
            .map(|a| ActivationRecord {
                from_version_no: a.from_version_id.and_then(|v| numbers.get(&v).copied()),
                to_version_no: numbers.get(&a.to_version_id).copied().unwrap_or_default(),
                activated_by: a.activated_by,
                activated_at: a.activated_at,
            })
            .collect(),
    ))
}

/// The tool-script authoring surface, with the gate it is authored behind.
///
/// Administrator only: a script is remote code, and authoring one is not something a token with
/// `write_metadata` may do. Executing the ACTIVE version stays open through `/tools`, which is a
/// different surface with a different gate.
pub fn script_routes() -> Router<AppState> {
    Router::new()
        .route("/tool_scripts", get(list_scripts).post(create_script))
        .route("/tool_scripts/{id}", get(get_script).patch(update_script))
        .route("/tool_scripts/draft_run", post(draft_run))
        .route(
            "/tool_scripts/{id}/formulas/draft_run",
            post(draft_run_formulas),
        )
        .route("/tool_scripts/inspect", post(inspect_script))
        .route("/tool_scripts/{id}/versions", post(create_version))
        .route("/tool_scripts/{id}/versions/{version_id}", get(get_version))
        .route(
            "/tool_scripts/{id}/versions/{version_id}/validate",
            post(validate_version),
        )
        .route(
            "/tool_scripts/{id}/versions/{version_id}/activate",
            post(activate_version),
        )
        .route("/tool_scripts/{id}/activations", get(list_activations))
        .route("/tool_scripts/{id}/version_usage", get(list_version_usage))
        .layer(middleware::from_fn(
            crate::common::middleware::require_admin,
        ))
        .merge(formula_set_route())
}

/// The formula set save, behind the gate a formula row is already written through.
///
/// A formula calculation is arithmetic over the catalog, not remote code, and its rows are CRUD
/// under `write_metadata` (Q196). Minting the set's version is the same act, so the save takes the
/// same callers; `created_by` on the version is the token's label, which resolves to the
/// administrator who minted the token.
fn formula_set_route() -> Router<AppState> {
    Router::new()
        .route("/tool_scripts/{id}/formulas", post(save_formula_set))
        .layer(middleware::from_fn(
            crate::common::middleware::require_admin_or_token_write_metadata,
        ))
}
