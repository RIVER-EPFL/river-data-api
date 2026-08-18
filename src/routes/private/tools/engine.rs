//! Generic dispatch for DB-stored tool scripts.
//!
//! A tool is a row in `tool_scripts` whose active `tool_script_versions` row carries the R
//! script, its manifest (typed inputs, outputs, constants, curve slots) and its test cases.
//! Calculation resolves constants and curves here, so the runner receives values and the
//! provenance snapshot is taken where the data was read, then proxies to the OpenCPU runner.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

/// The serialization the API requires of the runner: full precision (the default rounds to 4
/// significant digits), scalars as scalars, and R NA as null.
const RUNNER_JSON_ARGS: &str = "auto_unbox=true&digits=17&na=null";

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ManifestParam {
    pub name: String,
    pub label: String,
    pub kind: String,
    #[serde(default)]
    pub units: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    /// Free-text note when requiredness depends on a mode.
    #[serde(default)]
    pub when: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ManifestOutput {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub units: Option<String>,
    #[serde(default)]
    pub per_replicate: bool,
    #[serde(default)]
    pub aggregate_of: Option<String>,
    #[serde(default)]
    pub suggested_parameter_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ManifestCurve {
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Manifest {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub params: Vec<ManifestParam>,
    #[serde(default)]
    pub outputs: Vec<ManifestOutput>,
    #[serde(default)]
    pub constants: Vec<String>,
    #[serde(default)]
    pub curves: Vec<ManifestCurve>,
    #[serde(default)]
    pub match_keywords: Vec<String>,
}

/// One tool as `GET /tools` lists it: the manifest plus the identity of the version serving it.
#[derive(Debug, Serialize, ToSchema)]
pub struct ToolDescriptor {
    pub name: String,
    pub label: String,
    pub description: Option<String>,
    pub endpoint: String,
    pub params: Vec<ManifestParam>,
    pub outputs: Vec<ManifestOutput>,
    pub constants: Vec<String>,
    pub curves: Vec<ManifestCurve>,
    pub match_keywords: Vec<String>,
    pub script_version_id: Uuid,
    pub version_no: i32,
}

/// The exact code identity a result was produced by, recorded into the provenance blob.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ToolVersionRef {
    pub script_version_id: Uuid,
    pub version_no: i32,
    pub content_hash: String,
}

pub struct ActiveTool {
    pub script_id: Uuid,
    pub name: String,
    pub label: String,
    pub description: Option<String>,
    pub version_id: Uuid,
    pub version_no: i32,
    pub script: String,
    pub entry_function: String,
    pub content_hash: String,
    pub manifest: Manifest,
}

fn parse_manifest(name: &str, raw: serde_json::Value) -> AppResult<Manifest> {
    serde_json::from_value(raw)
        .map_err(|e| AppError::Internal(format!("tool '{name}' has an unreadable manifest: {e}")))
}

const ACTIVE_TOOL_SQL: &str = r"
    SELECT s.id AS script_id, s.name, s.label, s.description,
           v.id AS version_id, v.version_no, v.script, v.entry_function, v.manifest,
           v.content_hash
    FROM tool_scripts s
    JOIN tool_script_versions v ON v.id = s.active_version_id";

fn row_to_active(row: &sea_orm::QueryResult) -> AppResult<ActiveTool> {
    let name: String = row.try_get("", "name")?;
    let manifest = parse_manifest(&name, row.try_get("", "manifest")?)?;
    Ok(ActiveTool {
        script_id: row.try_get("", "script_id")?,
        label: row.try_get("", "label")?,
        description: row.try_get("", "description")?,
        version_id: row.try_get("", "version_id")?,
        version_no: row.try_get("", "version_no")?,
        script: row.try_get("", "script")?,
        entry_function: row.try_get("", "entry_function")?,
        content_hash: row.try_get("", "content_hash")?,
        manifest,
        name,
    })
}

pub async fn list_active_tools(db: &DatabaseConnection) -> AppResult<Vec<ActiveTool>> {
    let rows = db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("{ACTIVE_TOOL_SQL} ORDER BY s.name"),
        ))
        .await?;
    rows.iter().map(row_to_active).collect()
}

pub async fn find_active_tool(db: &DatabaseConnection, name: &str) -> AppResult<ActiveTool> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!("{ACTIVE_TOOL_SQL} WHERE LOWER(s.name) = LOWER($1)"),
            [name.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Unknown tool: {name}")))?;
    row_to_active(&row)
}

impl ActiveTool {
    pub fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.name.clone(),
            label: self.manifest.label.clone(),
            description: self
                .manifest
                .description
                .clone()
                .or_else(|| self.description.clone()),
            endpoint: format!("/api/tools/{}/calculate", self.name),
            params: self.manifest.params.clone(),
            outputs: self.manifest.outputs.clone(),
            constants: self.manifest.constants.clone(),
            curves: self.manifest.curves.clone(),
            match_keywords: self.manifest.match_keywords.clone(),
            script_version_id: self.version_id,
            version_no: self.version_no,
        }
    }

    pub fn version_ref(&self) -> ToolVersionRef {
        ToolVersionRef {
            script_version_id: self.version_id,
            version_no: self.version_no,
            content_hash: self.content_hash.clone(),
        }
    }
}

/// Whether a value fits a manifest `kind`. Arrays and grids stay shallow: their element shapes
/// are the wrapper's contract, this only rejects the wrong container.
fn kind_accepts(kind: &str, value: &serde_json::Value) -> bool {
    if let Some(variants) = kind.strip_prefix("enum:") {
        return value
            .as_str()
            .is_some_and(|s| variants.split('|').any(|v| v == s));
    }
    match kind {
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "array" | "replicate_grid" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
    }
}

/// A curve as the runner receives it: coefficients plus, when it came from the catalog, the
/// identity that resolves them.
#[derive(Debug, Serialize)]
struct ResolvedCurve {
    slope: f64,
    intercept: f64,
    standard_curve_id: Option<Uuid>,
    label: Option<String>,
}

async fn resolve_curve(
    db: &DatabaseConnection,
    slot: &ManifestCurve,
    value: &serde_json::Value,
) -> AppResult<ResolvedCurve> {
    let obj = value.as_object().ok_or_else(|| {
        AppError::BadRequest(format!(
            "curve '{}' must be an object with slope/intercept or standard_curve_id",
            slot.name
        ))
    })?;

    if let Some(id) = obj.get("standard_curve_id").and_then(|v| v.as_str()) {
        let id: Uuid = id
            .parse()
            .map_err(|_| AppError::BadRequest(format!("curve '{}': invalid UUID", slot.name)))?;
        let row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT slope, intercept, name FROM standard_curves WHERE id = $1",
                [id.into()],
            ))
            .await?
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "curve '{}': standard curve {id} not found",
                    slot.name
                ))
            })?;
        return Ok(ResolvedCurve {
            slope: row.try_get("", "slope")?,
            intercept: row.try_get("", "intercept")?,
            standard_curve_id: Some(id),
            label: row.try_get("", "name").ok(),
        });
    }

    let coeff = |key: &str| {
        obj.get(key)
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(|| {
                AppError::BadRequest(format!("curve '{}': {key} must be a number", slot.name))
            })
    };
    Ok(ResolvedCurve {
        slope: coeff("slope")?,
        intercept: coeff("intercept")?,
        standard_curve_id: None,
        label: obj
            .get("label")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

async fn resolve_constants(
    db: &DatabaseConnection,
    names: &[String],
) -> AppResult<serde_json::Map<String, serde_json::Value>> {
    let mut out = serde_json::Map::new();
    if names.is_empty() {
        return Ok(out);
    }
    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT name, value FROM constants WHERE name = ANY($1)",
            [names.to_vec().into()],
        ))
        .await?;
    for row in &rows {
        let name: String = row.try_get("", "name")?;
        let value: f64 = row.try_get("", "value")?;
        out.insert(name, serde_json::json!(value));
    }
    for name in names {
        if !out.contains_key(name) {
            return Err(AppError::Internal(format!(
                "constant '{name}' is missing from the constants table"
            )));
        }
    }
    Ok(out)
}

pub struct RunOutcome {
    pub results: serde_json::Map<String, serde_json::Value>,
    pub inputs_used: Vec<String>,
    pub inputs_ignored: Vec<String>,
    pub curves: Vec<serde_json::Value>,
    pub constants: serde_json::Map<String, serde_json::Value>,
}

/// Validate a request body against the tool's manifest, resolve its constants and curves, and
/// execute the script in the runner. Shared by `POST /tools/{name}/calculate` and version
/// validation (which supplies explicit constants/curves instead).
pub async fn run_active_tool(
    state: &AppState,
    tool: &ActiveTool,
    body: &[u8],
) -> AppResult<RunOutcome> {
    let body: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| AppError::BadRequest(format!("Invalid request body: {e}")))?;
    let mut body = match body {
        serde_json::Value::Object(map) => map,
        serde_json::Value::Null => serde_json::Map::new(),
        _ => {
            return Err(AppError::BadRequest(
                "request body must be a JSON object".to_string(),
            ));
        }
    };

    let manifest = &tool.manifest;
    let param_names: Vec<&str> = manifest.params.iter().map(|p| p.name.as_str()).collect();
    let curve_names: Vec<&str> = manifest.curves.iter().map(|c| c.name.as_str()).collect();

    for key in body.keys() {
        if !param_names.contains(&key.as_str()) && !curve_names.contains(&key.as_str()) {
            return Err(AppError::BadRequest(format!(
                "unknown field '{key}' for tool '{}'",
                tool.name
            )));
        }
    }
    for p in &manifest.params {
        if let Some(value) = body.get(&p.name)
            && !value.is_null()
            && !kind_accepts(&p.kind, value)
        {
            return Err(AppError::BadRequest(format!(
                "Invalid request body: field '{}' must be {} for tool '{}'",
                p.name, p.kind, tool.name
            )));
        }
    }
    for p in &manifest.params {
        let present = body.get(&p.name).is_some_and(|v| !v.is_null());
        if !present {
            if let Some(default) = &p.default {
                body.insert(p.name.clone(), default.clone());
            } else if p.required && p.when.is_none() {
                return Err(AppError::BadRequest(format!(
                    "missing required field '{}' for tool '{}'",
                    p.name, tool.name
                )));
            }
        }
    }

    let mut curves = serde_json::Map::new();
    let mut curve_snapshots = Vec::new();
    let mut curves_consumed: Vec<String> = Vec::new();
    for slot in &manifest.curves {
        match body.remove(&slot.name) {
            Some(value) if !value.is_null() => {
                curves_consumed.push(slot.name.clone());
                let resolved = resolve_curve(&state.db, slot, &value).await?;
                let json = serde_json::to_value(&resolved).unwrap_or_default();
                curve_snapshots.push(serde_json::json!({ "name": slot.name, "curve": json }));
                curves.insert(slot.name.clone(), json);
            }
            _ if slot.required => {
                return Err(AppError::BadRequest(format!(
                    "missing required curve '{}' for tool '{}'",
                    slot.name, tool.name
                )));
            }
            _ => {}
        }
    }

    let constants = resolve_constants(&state.db, &manifest.constants).await?;
    let provided: Vec<String> = body.keys().cloned().collect();

    let raw = execute_script(
        state,
        &tool.script,
        &tool.entry_function,
        &serde_json::Value::Object(body),
        &serde_json::Value::Object(constants.clone()),
        &serde_json::Value::Object(curves),
    )
    .await?;

    let mut results = match raw {
        serde_json::Value::Object(map) => map,
        // An R empty named list serialises as []: every output was omitted.
        serde_json::Value::Array(a) if a.is_empty() => serde_json::Map::new(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("value".to_string(), other);
            map
        }
    };
    // NA outputs arrive as null; absent is the contract for an uncomputable value.
    results.retain(|_, v| !v.is_null());

    let declared_used: Vec<String> = results
        .remove("inputs_used")
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let mut inputs_used: Vec<String> = if declared_used.is_empty() {
        provided
            .iter()
            .filter(|k| param_names.contains(&k.as_str()))
            .cloned()
            .collect()
    } else {
        declared_used
            .into_iter()
            .filter(|k| provided.contains(k))
            .collect()
    };
    // A curve that was sent and resolved was consumed by definition.
    inputs_used.extend(curves_consumed);
    let inputs_ignored: Vec<String> = provided
        .iter()
        .filter(|k| !inputs_used.contains(k))
        .cloned()
        .collect();

    Ok(RunOutcome {
        results,
        inputs_used,
        inputs_ignored,
        curves: curve_snapshots,
        constants,
    })
}

/// POST a script to the runner. Connection failures are the runner being down (503); a non-2xx
/// is the R error text, which is the script author's diagnostic.
pub async fn execute_script(
    state: &AppState,
    script: &str,
    entry: &str,
    inputs: &serde_json::Value,
    constants: &serde_json::Value,
    curves: &serde_json::Value,
) -> AppResult<serde_json::Value> {
    let Some(base) = state.config.tools_runner_url.as_deref() else {
        return Err(AppError::ServiceUnavailable(
            "the analytical tool runner is not configured (TOOLS_RUNNER_URL)".to_string(),
        ));
    };
    let url = format!("{base}/library/riverdata.tools/R/run_tool/json?{RUNNER_JSON_ARGS}");
    let payload = serde_json::json!({
        "script": script,
        "entry": entry,
        "inputs": inputs,
        "constants": constants,
        "curves": curves,
    });

    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let response = CLIENT
        .get_or_init(reqwest::Client::new)
        .post(&url)
        .timeout(std::time::Duration::from_secs(
            state.config.tools_runner_timeout_seconds,
        ))
        .json(&payload)
        .send()
        .await
        .map_err(|e| {
            AppError::ServiceUnavailable(format!("the analytical tool runner is unreachable: {e}"))
        })?;

    let status = response.status();
    let text = response.text().await.map_err(|e| {
        AppError::ServiceUnavailable(format!(
            "the analytical tool runner failed mid-response: {e}"
        ))
    })?;
    if !status.is_success() {
        // The first lines carry the R error message; the tail is the call echo.
        let message: String = text
            .lines()
            .take(4)
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();
        return Err(AppError::BadRequest(format!(
            "tool script error: {message}"
        )));
    }
    serde_json::from_str(&text)
        .map_err(|e| AppError::Internal(format!("the tool runner returned unparseable JSON: {e}")))
}
