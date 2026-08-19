//! Analytical tools: DB-stored, versioned R scripts executed by the OpenCPU runner.
//!
//! `engine` loads active versions and proxies calculation; `scripts` is the admin authoring
//! surface (versions, validation, activation). The portal calculation functions themselves live
//! inside the seeded scripts, verbatim.

pub mod engine;
pub mod scripts;

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Serialize;

use crate::common::AppState;
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
    /// The exact script version and runtime that produced these numbers; goes into the
    /// provenance blob on save.
    pub tool_version: ToolVersionRef,
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
    Path(tool_name): Path<String>,
    body: axum::body::Bytes,
) -> AppResult<Json<ToolResult>> {
    let tool = engine::find_active_tool(&state.db, &tool_name).await?;
    let outcome = engine::run_active_tool(&state, &tool, &body).await?;
    let runtime = engine::runner_runtime(&state).await;
    Ok(Json(ToolResult {
        tool: tool.name.clone(),
        results: serde_json::Value::Object(outcome.results),
        inputs_used: outcome.inputs_used,
        inputs_ignored: outcome.inputs_ignored,
        constants: serde_json::Value::Object(outcome.constants),
        curves: outcome.curves,
        tool_version: tool.version_ref(runtime.as_ref()),
    }))
}
