//! Analytical tools: DB-stored, versioned R scripts executed by the OpenCPU runner.
//!
//! `engine` loads active versions and proxies calculation; `scripts` is the admin authoring
//! surface (versions, validation, activation). The portal calculation functions themselves live
//! inside the seeded scripts, verbatim.

pub mod chain;
pub mod engine;
pub mod scripts;

use axum::{
    Extension, Json,
    extract::{Path, State},
};
use sea_orm::ConnectionTrait;
use serde::Serialize;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::AuthContext;
use crate::error::AppResult;
use engine::{ToolDescriptor, ToolVersionRef};

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ToolResult {
    pub tool: String,
    #[schema(value_type = Object)]
    pub results: serde_json::Value,
    pub inputs_used: Vec<String>,
    pub inputs_ignored: Vec<String>,
    /// The constant values the server resolved and passed to the runner, by name.
    #[schema(value_type = Object)]
    pub constants: serde_json::Value,
    /// The curves the server resolved, as the runner received them.
    #[schema(value_type = Vec<Object>)]
    pub curves: Vec<serde_json::Value>,
    /// Station properties resolved from the site named by `site_id`, as `{property, param, value}`.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    #[schema(value_type = Vec<Object>)]
    pub station_inputs: Vec<serde_json::Value>,
    /// Same-event parameter values resolved at `(site_id, collected_at)`, as
    /// `{param, parameter_code, parameter_id, value}`.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    #[schema(value_type = Vec<Object>)]
    pub event_inputs: Vec<serde_json::Value>,
    /// The exact script version and runtime that produced these numbers; goes into the
    /// provenance blob on save.
    pub tool_version: ToolVersionRef,
    /// The stored `tool_runs` row for this calculation. A grab save names it as `tool_run_id`
    /// and the server builds the provenance blob from that row, never from the client.
    pub run_id: Uuid,
}

/// List the active analytical tools with their full input/output manifests.
///
/// Each output carries its declaration plus `parameter`, the catalog row it resolves to here:
/// `parameter_id` first, then `suggested_parameter_code` case-insensitively, null when neither
/// names a row this database holds. `dangling_parameter_id` on the resolved parameter says the id
/// named a row that has since gone and the code carried the output instead. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/tools",
    responses(
        (status = 200, description = "List of tool descriptors", body = [ToolDescriptor]),
    ),
    tag = "tools"
)]
pub async fn list_tools(State(state): State<AppState>) -> AppResult<Json<Vec<ToolDescriptor>>> {
    let tools = engine::list_active_tools(&state.db).await?;
    let catalog =
        engine::load_parameter_catalog(&state.db, tools.iter().map(|tool| &tool.manifest)).await?;
    Ok(Json(
        tools.iter().map(|tool| tool.descriptor(&catalog)).collect(),
    ))
}

/// Run an analytical tool calculation. The body schema is the tool's manifest (call `GET /tools`);
/// unknown fields are refused by name. Requires `read_data`.
#[utoipa::path(
    post,
    path = "/tools/{tool_name}/calculate",
    params(("tool_name" = String, Path, description = "Tool name (e.g. 'doc', 'dic', 'pco2')")),
    request_body(content = Object, description = "Per-tool request body (see GET /tools for schemas)"),
    responses(
        (status = 200, description = "Calculation result with `inputs_used` / `inputs_ignored` accounting", body = ToolResult),
        (status = 404, description = "Unknown tool name"),
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
    let tool = engine::find_active_tool(&state.db, &tool_name).await?;
    let result = execute_and_store_run(&state, &tool, &body, &scripts::actor_label(&auth), "interactive").await?;
    Ok(Json(result))
}

/// Run a tool and store the `tool_runs` row that a later save references. Every path that
/// executes a tool for keeps goes through here — the interactive calculate endpoint, the CSV
/// tool-entry import, the chain executor — so the stored record has one shape.
///
/// The run row is written before the results are handed out: a save references the row, the
/// provenance blob is built from it, and every claim in the blob predates the save. The stored
/// inputs are the effective inputs the runner received (request values plus defaults and the
/// resolved station/event inputs), and `context` records where each resolved value came from.
pub async fn execute_and_store_run(
    state: &AppState,
    tool: &engine::ActiveTool,
    body: &[u8],
    actor: &str,
    source: &str,
) -> AppResult<ToolResult> {
    let outcome = engine::run_active_tool(state, tool, body).await?;
    let runtime = engine::runner_runtime(state).await;
    let tool_version = tool.version_ref(runtime.as_ref());
    let results = serde_json::Value::Object(outcome.results);
    let constants = serde_json::Value::Object(outcome.constants);

    let context = if outcome.site_id.is_some()
        || !outcome.station_inputs.is_empty()
        || !outcome.event_inputs.is_empty()
    {
        serde_json::json!({
            "site_id": outcome.site_id,
            "collected_at": outcome.collected_at,
            "station_inputs": outcome.station_inputs,
            "event_inputs": outcome.event_inputs,
        })
    } else {
        serde_json::Value::Null
    };

    let run_id = Uuid::new_v4();
    state
        .db
        .execute(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO tool_runs (id, tool_name, tool_version, inputs, constants, curves, \
             outputs, created_by, context, source) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            [
                run_id.into(),
                tool.name.clone().into(),
                serde_json::to_value(&tool_version)
                    .unwrap_or(serde_json::Value::Null)
                    .into(),
                serde_json::Value::Object(outcome.inputs).into(),
                constants.clone().into(),
                serde_json::Value::Array(outcome.curves.clone()).into(),
                results.clone().into(),
                actor.into(),
                context.into(),
                source.into(),
            ],
        ))
        .await?;

    Ok(ToolResult {
        tool: tool.name.clone(),
        results,
        inputs_used: outcome.inputs_used,
        inputs_ignored: outcome.inputs_ignored,
        constants,
        curves: outcome.curves,
        station_inputs: outcome.station_inputs,
        event_inputs: outcome.event_inputs,
        tool_version,
        run_id,
    })
}
