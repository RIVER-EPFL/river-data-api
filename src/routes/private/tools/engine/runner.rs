//! The OpenCPU runner transport: the runtime probe and its cached answer, the HTTP client, the
//! one call every runner request goes through, and the script inspections that are nothing but a
//! call and its typed answer.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

/// The serialization the API requires of the runner: full precision (the default rounds to 4
/// significant digits), scalars as scalars, and R NA as null.
const RUNNER_JSON_ARGS: &str = "auto_unbox=true&digits=17&na=null";

/// What the runner reports about itself. It cannot change without the container restarting, so
/// it is fetched once and held until a runner failure invalidates it.
#[derive(Debug, Clone)]
pub struct RunnerRuntime {
    pub runner_image: Option<String>,
    pub r_version: Option<String>,
}

#[derive(Deserialize)]
struct RuntimeInfoResponse {
    #[serde(default)]
    r_version: Option<String>,
    #[serde(default)]
    image_build: Option<String>,
}

fn runtime_cell() -> &'static tokio::sync::RwLock<Option<RunnerRuntime>> {
    static CELL: std::sync::OnceLock<tokio::sync::RwLock<Option<RunnerRuntime>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| tokio::sync::RwLock::new(None))
}

pub async fn invalidate_runner_runtime() {
    *runtime_cell().write().await = None;
}

pub async fn runner_runtime(state: &AppState) -> Option<RunnerRuntime> {
    if let Some(cached) = runtime_cell().read().await.clone() {
        return Some(cached);
    }
    let fetched = fetch_runtime_info(state).await?;
    *runtime_cell().write().await = Some(fetched.clone());
    Some(fetched)
}

async fn fetch_runtime_info(state: &AppState) -> Option<RunnerRuntime> {
    let base = state.config.tools_runner_url.as_deref()?;
    let url = format!("{base}/library/riverdata.tools/R/runtime_info/json?auto_unbox=true");
    let response = runner_client()
        .post(&url)
        .timeout(std::time::Duration::from_secs(
            state.config.tools_runner_timeout_seconds,
        ))
        .json(&serde_json::json!({}))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let info: RuntimeInfoResponse = response.json().await.ok()?;
    Some(RunnerRuntime {
        runner_image: info.image_build,
        r_version: info.r_version,
    })
}

fn runner_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// Where a script failed to parse. `line`/`column` are absent when R's message carries no
/// position.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct ParseError {
    pub message: String,
    #[serde(default)]
    pub line: Option<i64>,
    #[serde(default)]
    pub column: Option<i64>,
}

/// A detection the parse tree cannot complete: `any` is what a caller branches on, the
/// expressions are what it shows when it does.
#[derive(Debug, Clone, Default, Deserialize, Serialize, ToSchema)]
pub struct DynamicFlag {
    pub any: bool,
    pub expressions: Vec<String>,
}

/// What the runner reads off a script's parse tree without evaluating it.
///
/// Every list is a floor rather than a complete set. A script that assembles names at runtime
/// (`out[[paste0(base, rep)]] <- ...`, the per-replicate outputs) cannot be read statically, and
/// that is what `dynamic_outputs` and `dynamic_reads` report: while either `any` is true, the
/// corresponding list is known to be short by an unknown amount.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct ScriptInspection {
    pub parse_ok: bool,
    /// Null when the script parses. A syntax error is a normal result, not a failed request.
    #[serde(default, deserialize_with = "deserialize_parse_error")]
    pub parse_error: Option<ParseError>,
    pub entry: String,
    pub entry_found: bool,
    /// The entry function's formals in declaration order; the runner calls them positionally.
    pub entry_args: Vec<String>,
    pub inputs: Vec<String>,
    pub constants: Vec<String>,
    pub curves: Vec<String>,
    /// The output keys read off the entry function. A floor: see `dynamic_outputs`.
    pub outputs: Vec<String>,
    pub dynamic_outputs: DynamicFlag,
    pub dynamic_reads: DynamicFlag,
    pub functions_defined: Vec<String>,
    pub functions_called: Vec<String>,
    /// The script's own top-level functions the entry function calls, which is what a tool
    /// depends on out of its prelude.
    pub script_functions_used: Vec<String>,
    pub libraries: Vec<String>,
    pub namespaces: Vec<String>,
}

/// R's empty list serialises as `[]`, which here means "no error" rather than a malformed one.
fn deserialize_parse_error<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<Option<ParseError>, D::Error> {
    let value = serde_json::Value::deserialize(de)?;
    if value.is_array() || value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

impl ScriptInspection {
    /// Whether the detected output list can be read as complete.
    #[must_use]
    pub fn outputs_complete(&self) -> bool {
        !self.dynamic_outputs.any
    }
}

/// Read a script's parse tree in the runner. Nothing is evaluated, so a hostile or half-written
/// script is safe to inspect and a syntax error comes back as `parse_ok = false`.
pub async fn inspect_script(
    state: &AppState,
    script: &str,
    entry: &str,
) -> AppResult<ScriptInspection> {
    let raw = call_runner(
        state,
        "inspect_script",
        &serde_json::json!({ "script": script, "entry": entry }),
    )
    .await?;
    serde_json::from_value(raw).map_err(|e| {
        AppError::Internal(format!(
            "the tool runner returned an unreadable inspection: {e}"
        ))
    })
}

/// One call head the runner read off the parse tree, or one symbol read in value position.
/// A namespaced head arrives composed, as `pkg::fn`.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct ScannedName {
    pub name: String,
    pub line: i64,
}

/// One argument of one call. `name` is the argument's name where it had one, `kind` is
/// `string`, `symbol` or `other`, and `value` carries the literal or the symbol behind the first
/// two. A `library("curl")` and a `cat(f = "out.txt")` are both readable from this without
/// re-parsing the source.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct ScannedArg {
    pub call: String,
    pub name: String,
    pub value: String,
    pub kind: String,
    pub line: i64,
}

/// A script's call structure with line numbers, which is what the safety lint applies its policy
/// to. The runner reports structure only; which names are refused lives in `scripts.rs`.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct ScriptScan {
    pub parse_ok: bool,
    /// Null when the script parses. A syntax error is a normal result, not a failed request.
    #[serde(default, deserialize_with = "deserialize_parse_error")]
    pub parse_error: Option<ParseError>,
    #[serde(default)]
    pub calls: Vec<ScannedName>,
    /// Symbols read in value position that the script does not itself bind, which is where an
    /// alias (`runner <- system`) is visible.
    #[serde(default)]
    pub symbols: Vec<ScannedName>,
    #[serde(default)]
    pub args: Vec<ScannedArg>,
}

/// Read a script's call structure in the runner. Nothing is evaluated: `parse()` builds the tree
/// and the walk reads it, so scanning a hostile script is as safe as reading it.
pub async fn scan_script(state: &AppState, script: &str) -> AppResult<ScriptScan> {
    let raw = call_runner(
        state,
        "scan_script",
        &serde_json::json!({ "script": script }),
    )
    .await?;
    serde_json::from_value(raw).map_err(|e| {
        AppError::Internal(format!("the tool runner returned an unreadable scan: {e}"))
    })
}

/// The runner's syntax check on its own. `ok` with no message is a script that parses.
#[derive(Debug, Clone, Deserialize)]
pub struct ParseCheck {
    pub ok: bool,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub line: Option<i64>,
    #[serde(default)]
    pub column: Option<i64>,
}

pub async fn parse_check(state: &AppState, script: &str) -> AppResult<ParseCheck> {
    let raw = call_runner(
        state,
        "parse_check",
        &serde_json::json!({ "script": script }),
    )
    .await?;
    serde_json::from_value(raw).map_err(|e| {
        AppError::Internal(format!(
            "the tool runner returned an unreadable parse check: {e}"
        ))
    })
}

/// The runner's signal that the failure came from inside the tool: one JSON line on the first
/// line of a non-2xx body.
#[derive(Deserialize)]
struct RunnerToolError {
    error: String,
    message: String,
    #[serde(default)]
    call: Option<String>,
    #[serde(default)]
    traceback: Vec<String>,
}

fn parse_tool_error(body: &str) -> Option<RunnerToolError> {
    let first = body.lines().next()?;
    let parsed: RunnerToolError = serde_json::from_str(first).ok()?;
    (parsed.error == "tool_error").then_some(parsed)
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
    call_runner(
        state,
        "run_tool",
        &serde_json::json!({
            "script": script,
            "entry": entry,
            "inputs": inputs,
            "constants": constants,
            "curves": curves,
        }),
    )
    .await
}

/// POST one `riverdata.tools` function. Every runner call goes through here so the URL, the
/// mandatory JSON arguments, the shared client, the timeout and the failure vocabulary are
/// decided once.
async fn call_runner(
    state: &AppState,
    function: &str,
    payload: &serde_json::Value,
) -> AppResult<serde_json::Value> {
    let Some(base) = state.config.tools_runner_url.as_deref() else {
        return Err(AppError::ServiceUnavailable(
            "the analytical tool runner is not configured (TOOLS_RUNNER_URL)".to_string(),
        ));
    };
    let url = format!("{base}/library/riverdata.tools/R/{function}/json?{RUNNER_JSON_ARGS}");

    let response = runner_client()
        .post(&url)
        .timeout(std::time::Duration::from_secs(
            state.config.tools_runner_timeout_seconds,
        ))
        .json(payload)
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(e) => {
            // The container may have restarted, so its reported runtime is no longer trusted.
            invalidate_runner_runtime().await;
            return Err(AppError::ServiceUnavailable(format!(
                "the analytical tool runner is unreachable: {e}"
            )));
        }
    };

    let status = response.status();
    let text = match response.text().await {
        Ok(text) => text,
        Err(e) => {
            invalidate_runner_runtime().await;
            return Err(AppError::ServiceUnavailable(format!(
                "the analytical tool runner failed mid-response: {e}"
            )));
        }
    };
    if !status.is_success() {
        if let Some(failure) = parse_tool_error(&text) {
            return Err(AppError::ToolScriptError {
                message: failure.message,
                call: failure.call,
                traceback: failure.traceback,
            });
        }
        // OpenCPU's own plain-text errors, raised before the tool is entered. The first lines
        // carry the R error message; the tail is the call echo.
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

#[cfg(test)]
mod tests {
    use super::parse_tool_error;

    #[test]
    fn opencpu_plain_text_is_not_a_tool_error() {
        let body = "unused argument (nope = 1)\n\nIn call:\nrun_tool(nope = 1L)";
        assert!(parse_tool_error(body).is_none());
    }

    #[test]
    fn the_marker_line_carries_message_call_and_traceback() {
        let body = concat!(
            r#"{"error":"tool_error","message":"boom","call":"fn(x)","traceback":["fn(x)"]}"#,
            "\nBacktrace:\n  1. eval(call)"
        );
        let parsed = parse_tool_error(body).expect("first line parses");
        assert_eq!(parsed.message, "boom");
        assert_eq!(parsed.call.as_deref(), Some("fn(x)"));
        assert_eq!(parsed.traceback, vec!["fn(x)".to_string()]);
    }

    #[test]
    fn a_json_line_without_the_marker_falls_through() {
        let body = r#"{"error":"other","message":"boom"}"#;
        assert!(parse_tool_error(body).is_none());
    }
}
