//! Admin authoring surface for DB-stored tool scripts.
//!
//! Versions are immutable: creating one appends, activation flips the script's pointer (recorded
//! in `tool_script_activations`), and rollback is activating an older version. A version goes live
//! only when its stored test cases pass against the runner at that moment, never on the strength
//! of a stamp left by an earlier run.

use axum::{
    Extension, Json,
    extract::{Path, State},
};
use sea_orm::{ConnectionTrait, Statement, TransactionTrait};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::engine;
// One hash rule for every version, shared with the seed migration that installs the shipped tools.
pub use migration::tool_hash::{stored_version_content, version_content_hash};

use crate::common::AppState;
use crate::common::middleware::AuthContext;
use crate::error::{AppError, AppResult};

/// Who the audit columns record. Taken from the authenticated caller, never from the request:
/// a self-asserted author or activator is not a trail. These routes are Administrator-only, so
/// the token arm exists for exhaustiveness rather than for a caller that can arrive here.
pub(crate) fn actor_label(auth: &AuthContext) -> String {
    match auth {
        AuthContext::Keycloak { email: Some(e), .. } => e.clone(),
        AuthContext::Keycloak { sub, .. } => sub.clone(),
        AuthContext::ApiToken { token_id, .. } => format!("token:{token_id}"),
    }
}

/// Packages a script may load or reach into with `::`. Mirrors the runner image's installed set
/// plus the runner's own package; anything else fails at run time anyway, the lint just says so
/// earlier.
const LIBRARY_WHITELIST: &[&str] = &[
    "dplyr",
    "tidyr",
    "magrittr",
    "pracma",
    "signal",
    "bigleaf",
    "stats",
    "utils",
    "methods",
    "riverdata.tools",
    "base",
];

/// Calls that load or attach a package, whose argument names the package.
const PACKAGE_LOADERS: &[&str] = &[
    "library",
    "require",
    "requireNamespace",
    "loadNamespace",
    "attachNamespace",
];

/// Calls that resolve a function from a name given as a string, which is how a forbidden call is
/// reached without ever being written as one.
const NAME_RESOLVERS: &[&str] = &[
    "do.call",
    "get",
    "get0",
    "mget",
    "match.fun",
    "getFunction",
    "getExportedValue",
    "getFromNamespace",
    "getAnywhere",
];

/// Calls that take a function as an argument and accept its name as a string, because they pass it
/// through `match.fun`. `lapply(x, "system")` calls `system` without the tree ever holding a call
/// to it, so their string arguments are read the way a resolver's are.
const FUNCTION_ARGS: &[&str] = &[
    "lapply",
    "sapply",
    "vapply",
    "mapply",
    "Map",
    "Reduce",
    "Filter",
    "Find",
    "Position",
    "apply",
    "tapply",
    "rapply",
    "eapply",
    "by",
    "outer",
    "aggregate",
    "sweep",
    "Negate",
    "Vectorize",
];

/// Names whose only uses in a tool script are accidents, and what each one reaches.
///
/// **The runner container is the security boundary.** It holds no database, no secrets and no
/// route to the network, so a script that gets past this list still reaches nothing. What follows
/// is accident protection with a line number, not a sandbox.
///
/// It is applied to the parse tree rather than to the source text because R spells one call many
/// ways: `system ("ls")`, `` `system`() ``, `base::system()`, `do.call("system", ...)`,
/// `get("system")()`, or an alias assigned first. A scan over characters has to anticipate each
/// spelling separately and misreads a raw string literal on top; the tree has already resolved
/// all of them to one call head or one string argument.
const FORBIDDEN: &[(&str, &str)] = &[
    ("system", "shell execution"),
    ("system2", "shell execution"),
    ("shell", "shell execution"),
    ("pipe", "shell execution"),
    ("socketConnection", "network access"),
    ("download.file", "network access"),
    ("url", "network access"),
    ("install.packages", "package installation"),
    ("Sys.setenv", "environment mutation"),
    ("Sys.chmod", "file permission changes"),
    ("Sys.umask", "file permission changes"),
    ("file.remove", "file deletion"),
    ("unlink", "file deletion"),
    ("writeLines", "file writes"),
    ("writeChar", "file writes"),
    ("writeBin", "file writes"),
    ("write", "file writes"),
    ("write.csv", "file writes"),
    ("write.csv2", "file writes"),
    ("write.table", "file writes"),
    ("sink", "output redirection"),
    ("capture.output", "output redirection"),
    ("saveRDS", "file writes"),
    ("save", "file writes"),
    ("save.image", "file writes"),
    ("file.create", "file creation"),
    ("file.copy", "file creation"),
    ("file.rename", "file creation"),
    ("file.append", "file writes"),
    ("file.link", "file creation"),
    ("file.symlink", "file creation"),
    ("dir.create", "directory creation"),
    ("file", "file connections"),
    ("gzfile", "file connections"),
    ("bzfile", "file connections"),
    ("xzfile", "file connections"),
    ("fifo", "file connections"),
    ("unz", "file connections"),
    (".Internal", "internal calls"),
    (".Call", "native calls"),
    (".External", "native calls"),
    (".C", "native calls"),
    (".Fortran", "native calls"),
    ("quit", "session control"),
    ("eval", "dynamic evaluation"),
    ("evalq", "dynamic evaluation"),
    ("parse", "dynamic evaluation"),
    ("str2lang", "dynamic evaluation"),
    ("str2expression", "dynamic evaluation"),
    ("source", "dynamic evaluation"),
    ("sys.source", "dynamic evaluation"),
    ("assign", "dynamic evaluation"),
    ("attach", "dynamic evaluation"),
    ("do.call", "dynamic name resolution"),
    ("get", "dynamic name resolution"),
    ("get0", "dynamic name resolution"),
    ("mget", "dynamic name resolution"),
    ("match.fun", "dynamic name resolution"),
    ("getFunction", "dynamic name resolution"),
    ("getExportedValue", "dynamic name resolution"),
    ("getFromNamespace", "dynamic name resolution"),
    ("getAnywhere", "dynamic name resolution"),
    // An environment handed back as a value is a namespace the scan cannot follow: `baseenv()$f`
    // and `asNamespace("base")$f` reach every name in base under a field read. A bench calculation
    // has no use for one.
    ("asNamespace", "environment access"),
    ("getNamespace", "environment access"),
    ("loadedNamespaces", "environment access"),
    ("baseenv", "environment access"),
    ("globalenv", "environment access"),
    ("topenv", "environment access"),
    ("as.environment", "environment access"),
    ("parent.env", "environment access"),
    ("sys.function", "environment access"),
    ("environment", "environment access"),
];

/// Calls with an argument that opens a file, named in full. R matches an argument name by prefix,
/// so the lint matches by prefix too: `cat(f = "out.txt")` is `cat(file = "out.txt")`.
const PARTIAL_FILE_ARGS: &[(&str, &str)] = &[("cat", "file")];

#[derive(Debug, Serialize, ToSchema)]
pub struct LintFinding {
    /// The script line the finding sits on. Zero for a finding about the manifest, which has no
    /// line in the script the editor shows.
    pub line: usize,
    pub message: String,
}

/// A manifest finding, carried in the same shape as a script finding so a caller renders one list.
fn manifest_finding(message: String) -> LintFinding {
    LintFinding { line: 0, message }
}

fn forbidden_reason(name: &str) -> Option<&'static str> {
    FORBIDDEN
        .iter()
        .find(|(forbidden, _)| *forbidden == name)
        .map(|(_, why)| *why)
}

fn line_of(line: i64) -> usize {
    usize::try_from(line).unwrap_or(1).max(1)
}

fn refusal(line: i64, name: &str, why: &str) -> LintFinding {
    LintFinding {
        line: line_of(line),
        message: format!("'{name}' is not allowed in tool scripts ({why})"),
    }
}

/// What one name the script reaches is worth, called or merely read.
///
/// A namespaced name is three separate questions: whether it reaches package internals, whether the
/// package is in the image at all, and whether the function it names is refused. All three are
/// asked of `pkg::fn` and only the last of a bare name.
fn push_name_findings(findings: &mut Vec<LintFinding>, reference: &engine::ScannedName) {
    if let Some((package, function)) = reference.name.split_once("::") {
        let function = function.trim_start_matches(':');
        if reference.name.contains(":::") {
            findings.push(LintFinding {
                line: line_of(reference.line),
                message: format!(
                    "reaching '{package}' internals with ':::' is not allowed in tool scripts \
                     (internal calls)"
                ),
            });
        }
        if !LIBRARY_WHITELIST.contains(&package) {
            findings.push(LintFinding {
                line: line_of(reference.line),
                message: format!("package '{package}' is not in the runner image"),
            });
        }
        if let Some(why) = forbidden_reason(function) {
            findings.push(refusal(reference.line, function, why));
        }
        return;
    }
    if let Some(why) = forbidden_reason(&reference.name) {
        findings.push(refusal(reference.line, &reference.name, why));
    }
}

/// The findings a scanned script yields, each carrying the line the runner read it off.
///
/// Every rule reads the parse tree, so a spelling that reaches a name reports that name: an alias
/// is reported where it is assigned, a string handed to `get` is reported as the function it
/// names, and a raw string literal is a string rather than something that desynchronises the scan.
///
/// What it cannot report is a name that does not exist until the script runs. `f(paste0("sys",
/// "tem"))` holds no name for any rule to read, and no static pass over a language with `eval` can
/// change that. The rules above make the ordinary spellings of a mistake visible with a line
/// number; a determined author still reaches arbitrary R, which is why the container the script
/// runs in, and not this list, is what holds nothing worth reaching.
fn findings_from_scan(scan: &engine::ScriptScan) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    // Calls and symbols are the same question asked at two positions: which function does this name
    // reach. `base::system(...)` and `runner <- base::system` differ only in when it is invoked, so
    // the namespace is read off both the same way.
    let named = scan.calls.iter().chain(scan.symbols.iter());
    for reference in named {
        push_name_findings(&mut findings, reference);
    }
    for arg in &scan.args {
        if PACKAGE_LOADERS.contains(&arg.call.as_str())
            && (arg.kind == "string" || arg.kind == "symbol")
            && !LIBRARY_WHITELIST.contains(&arg.value.as_str())
        {
            findings.push(LintFinding {
                line: line_of(arg.line),
                message: format!("package '{}' is not in the runner image", arg.value),
            });
        }
        // A string in either position is a function name R will resolve, so it is read as the
        // function rather than as text.
        if (NAME_RESOLVERS.contains(&arg.call.as_str())
            || FUNCTION_ARGS.contains(&arg.call.as_str()))
            && arg.kind == "string"
            && let Some(why) = forbidden_reason(&arg.value)
        {
            findings.push(refusal(arg.line, &arg.value, why));
        }
        if !arg.name.is_empty()
            && PARTIAL_FILE_ARGS
                .iter()
                .any(|(call, full)| *call == arg.call && full.starts_with(arg.name.as_str()))
        {
            findings.push(LintFinding {
                line: line_of(arg.line),
                message: format!(
                    "'{}' with a file= argument is not allowed in tool scripts (file writes)",
                    arg.call
                ),
            });
        }
    }
    findings.sort_by(|a, b| a.line.cmp(&b.line).then_with(|| a.message.cmp(&b.message)));
    findings.dedup_by(|a, b| a.line == b.line && a.message == b.message);
    findings
}

/// Lint a script against the parse tree the runner reads off it.
///
/// A script that does not parse yields the syntax error and nothing else: there is no tree to
/// apply the policy to, and naming constructs found in an unparseable file would be a guess.
///
/// A runner that is down or unconfigured is an error rather than an empty finding list: the lint
/// is unavailable, which is not the same as passed, and what that costs is the caller's to decide.
async fn lint_script(state: &AppState, script: &str) -> AppResult<Vec<LintFinding>> {
    let scan = engine::scan_script(state, script).await?;
    if !scan.parse_ok {
        let error = scan.parse_error.as_ref();
        let message = error.map_or("syntax error", |e| e.message.trim());
        return Ok(vec![LintFinding {
            // R names no position for some conditions; the first line is where an author looks then.
            line: error.and_then(|e| e.line).map_or(1, line_of),
            message: format!("the script does not parse as R: {message}"),
        }]);
    }
    Ok(findings_from_scan(&scan))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ToolScriptSummary {
    pub id: Uuid,
    pub name: String,
    pub label: String,
    pub description: Option<String>,
    pub active_version_id: Option<Uuid>,
    pub active_version_no: Option<i32>,
    pub version_count: i64,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// List every tool script with its active version. Requires Administrator.
#[utoipa::path(get, path = "/tool_scripts",
    responses((status = 200, body = [ToolScriptSummary])), tag = "tool_scripts")]
pub async fn list_scripts(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<ToolScriptSummary>>> {
    let rows = state
        .db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT s.id, s.name, s.label, s.description, s.active_version_id, s.updated_at,
                     av.version_no AS active_version_no,
                     (SELECT count(*) FROM tool_script_versions v
                       WHERE v.tool_script_id = s.id) AS version_count
              FROM tool_scripts s
              LEFT JOIN tool_script_versions av ON av.id = s.active_version_id
              ORDER BY s.name"
                .to_string(),
        ))
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(ToolScriptSummary {
            id: row.try_get("", "id")?,
            name: row.try_get("", "name")?,
            label: row.try_get("", "label")?,
            description: row.try_get("", "description")?,
            active_version_id: row.try_get("", "active_version_id")?,
            active_version_no: row.try_get("", "active_version_no")?,
            version_count: row.try_get("", "version_count")?,
            updated_at: row.try_get("", "updated_at")?,
        });
    }
    Ok(Json(out))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VersionSummary {
    pub id: Uuid,
    pub version_no: i32,
    pub content_hash: String,
    pub entry_function: String,
    /// What changed in this version and why, as its author wrote it.
    pub note: Option<String>,
    pub created_by: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub validated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub active: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ToolScriptDetail {
    #[serde(flatten)]
    pub summary: ToolScriptSummary,
    pub versions: Vec<VersionSummary>,
}

async fn load_summary(state: &AppState, id: Uuid) -> AppResult<ToolScriptSummary> {
    let row = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT s.id, s.name, s.label, s.description, s.active_version_id, s.updated_at,
                     av.version_no AS active_version_no,
                     (SELECT count(*) FROM tool_script_versions v
                       WHERE v.tool_script_id = s.id) AS version_count
              FROM tool_scripts s
              LEFT JOIN tool_script_versions av ON av.id = s.active_version_id
              WHERE s.id = $1",
            [id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound(format!("tool script {id} not found")))?;
    Ok(ToolScriptSummary {
        id: row.try_get("", "id")?,
        name: row.try_get("", "name")?,
        label: row.try_get("", "label")?,
        description: row.try_get("", "description")?,
        active_version_id: row.try_get("", "active_version_id")?,
        active_version_no: row.try_get("", "active_version_no")?,
        version_count: row.try_get("", "version_count")?,
        updated_at: row.try_get("", "updated_at")?,
    })
}

/// One script with its version history, newest first. Requires Administrator.
#[utoipa::path(get, path = "/tool_scripts/{id}", params(("id" = Uuid, Path)),
    responses((status = 200, body = ToolScriptDetail)), tag = "tool_scripts")]
pub async fn get_script(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<ToolScriptDetail>> {
    let summary = load_summary(&state, id).await?;
    let rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id, version_no, content_hash, entry_function, note, created_by, created_at,
                     validated_at
              FROM tool_script_versions WHERE tool_script_id = $1 ORDER BY version_no DESC",
            [id.into()],
        ))
        .await?;
    let mut versions = Vec::with_capacity(rows.len());
    for row in &rows {
        let vid: Uuid = row.try_get("", "id")?;
        versions.push(VersionSummary {
            id: vid,
            version_no: row.try_get("", "version_no")?,
            content_hash: row.try_get("", "content_hash")?,
            entry_function: row.try_get("", "entry_function")?,
            note: row.try_get("", "note")?,
            created_by: row.try_get("", "created_by")?,
            created_at: row.try_get("", "created_at")?,
            validated_at: row.try_get("", "validated_at")?,
            active: summary.active_version_id == Some(vid),
        });
    }
    Ok(Json(ToolScriptDetail { summary, versions }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateScriptRequest {
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Create a tool script (no versions yet; it lists in `GET /tools` only once a version is
/// activated). `created_by` is the authenticated caller. Requires Administrator.
#[utoipa::path(post, path = "/tool_scripts", request_body = CreateScriptRequest,
    responses((status = 200, body = ToolScriptSummary)), tag = "tool_scripts")]
pub async fn create_script(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Json(payload): Json<CreateScriptRequest>,
) -> AppResult<Json<ToolScriptSummary>> {
    let name = payload.name.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(AppError::BadRequest(
            "tool name must be non-empty [a-z0-9_]".to_string(),
        ));
    }
    let row = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"INSERT INTO tool_scripts (name, label, description, created_by)
              VALUES ($1, $2, $3, $4) RETURNING id",
            [
                name.to_lowercase().into(),
                payload.label.into(),
                payload.description.into(),
                actor_label(&auth).into(),
            ],
        ))
        .await
        .map_err(|e| {
            if e.to_string().contains("idx_tool_scripts_name") {
                AppError::Conflict(format!("a tool named '{name}' already exists"))
            } else {
                AppError::Database(e)
            }
        })?
        .ok_or_else(|| AppError::Internal("insert returned no row".to_string()))?;
    let id: Uuid = row.try_get("", "id")?;
    Ok(Json(load_summary(&state, id).await?))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateScriptRequest {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Update a script's label/description (the code lives in versions). Requires Administrator.
#[utoipa::path(patch, path = "/tool_scripts/{id}", params(("id" = Uuid, Path)),
    request_body = UpdateScriptRequest,
    responses((status = 200, body = ToolScriptSummary)), tag = "tool_scripts")]
pub async fn update_script(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateScriptRequest>,
) -> AppResult<Json<ToolScriptSummary>> {
    state
        .db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE tool_scripts SET label = COALESCE($2, label),
                     description = COALESCE($3, description), updated_at = now()
              WHERE id = $1",
            [id.into(), payload.label.into(), payload.description.into()],
        ))
        .await?;
    Ok(Json(load_summary(&state, id).await?))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateVersionRequest {
    pub script: String,
    #[serde(default)]
    pub entry_function: Option<String>,
    #[schema(value_type = Object)]
    pub manifest: serde_json::Value,
    #[serde(default)]
    #[schema(value_type = Object)]
    pub test_cases: Option<serde_json::Value>,
    /// Short free text: what changed in this version and why.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CreateVersionResponse {
    pub version: VersionSummary,
    /// What was stored anyway but is worth saying: an output whose `suggested_parameter_code`
    /// matches no catalog parameter. Empty on a version with nothing to report.
    pub lint: Vec<LintFinding>,
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
#[utoipa::path(post, path = "/tool_scripts/{id}/versions", params(("id" = Uuid, Path)),
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
    load_summary(&state, id).await?;

    // The manifest is refused by rules this API owns and the lint is read off the runner, so the
    // manifest is settled first: a misspelled kind names its field whether or not the sidecar is
    // up, instead of the outage answering for it.
    //
    // The stored manifest is this value, not the payload: an output naming only an id has the
    // resolved code written into it here, so the version carries the half that travels.
    let mut stored_manifest = payload.manifest.clone();
    let mut manifest = engine::parse_manifest(&stored_manifest)
        .map_err(|e| AppError::BadRequest(format!("invalid manifest: {e}")))?;
    let catalog = engine::check_manifest_against_catalog(
        &state.db,
        &mut manifest,
        Some(&mut stored_manifest),
        engine::MissingConstant::Refuse,
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
    let stored = migration::tool_hash::stored_version_content(
        &state.db,
        &payload.script,
        &entry,
        &stored_manifest,
        &test_cases,
    )
    .await?;
    let row = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"INSERT INTO tool_script_versions
                  (tool_script_id, version_no, script, entry_function, manifest, test_cases,
                   content_hash, note, created_by)
              SELECT $1,
                     COALESCE((SELECT max(version_no) FROM tool_script_versions
                               WHERE tool_script_id = $1), 0) + 1,
                     $2, $3, $4::jsonb, $5::jsonb, $6, $7, $8
              RETURNING id",
            [
                id.into(),
                payload.script.into(),
                entry.into(),
                stored.manifest.into(),
                stored.test_cases.into(),
                stored.content_hash.into(),
                payload.note.into(),
                actor_label(&auth).into(),
            ],
        ))
        .await
        .map_err(|e| {
            if e.to_string().contains("content_hash") {
                AppError::Conflict("an identical version of this script already exists".to_string())
            } else {
                AppError::Database(e)
            }
        })?
        .ok_or_else(|| AppError::Internal("insert returned no row".to_string()))?;
    let vid: Uuid = row.try_get("", "id")?;

    let detail = get_script(State(state), Path(id)).await?.0;
    let version = detail
        .versions
        .into_iter()
        .find(|v| v.id == vid)
        .ok_or_else(|| AppError::Internal("created version not found".to_string()))?;
    Ok(Json(CreateVersionResponse {
        version,
        lint: warnings,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct DraftRunRequest {
    pub script: String,
    #[serde(default)]
    pub entry_function: Option<String>,
    #[schema(value_type = Object)]
    pub manifest: serde_json::Value,
    /// The calculate request body this draft is run with: the manifest's params, plus its curve
    /// slots given either as a `standard_curve_id` or as literal coefficients.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub inputs: Option<serde_json::Value>,
    /// Constant values in place of the catalog. As in a stored test case, an override must name
    /// every constant the manifest declares; omit the field to read the catalog.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub constants: Option<serde_json::Map<String, serde_json::Value>>,
}

/// What the script produced, present only when the run reached the end.
#[derive(Debug, Serialize, ToSchema)]
pub struct DraftRunResults {
    #[schema(value_type = Object)]
    pub results: serde_json::Value,
    pub inputs_used: Vec<String>,
    pub inputs_ignored: Vec<String>,
    #[schema(value_type = Object)]
    pub constants: serde_json::Value,
    #[schema(value_type = Vec<Object>)]
    pub curves: Vec<serde_json::Value>,
}

/// Which of the three things that end a draft run happened, so a caller can render the failure
/// where it belongs (the body form, the script pane, or the runner's own state) instead of
/// reading the message.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DraftRunFailureKind {
    /// The manifest refused the request body: an undeclared field, a wrong kind, a missing
    /// requirement, or a curve that does not resolve.
    BodyRefused,
    /// The script raised. `call` and `traceback` carry what R reported.
    ScriptError,
    /// The runner is unconfigured or did not answer.
    RunnerUnavailable,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DraftRunFailure {
    pub kind: DraftRunFailureKind,
    pub message: String,
    /// The R call that raised, when the runner named one.
    pub call: Option<String>,
    pub traceback: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DraftRunResponse {
    /// True when `results` and the rest of the run fields are present; false when `failure` is.
    pub ran: bool,
    #[serde(flatten)]
    pub run: Option<DraftRunResults>,
    /// Why the run ended without results. A failure here is a finding about the draft, not a
    /// failed request, so it travels at 200 next to `lint`.
    pub failure: Option<DraftRunFailure>,
    /// Carries the runner the request reached; the version fields are null, nothing here is
    /// stored.
    pub tool_version: engine::ToolVersionRef,
    /// What the save path would say about this script. Findings neither stop a draft from running
    /// nor depend on it running: the runner container is the boundary either way, and an author
    /// mid-edit wants everything that is wrong in one answer. `POST /tool_scripts/{id}/versions`
    /// still refuses to store them. A constant the table does not hold is reported here and left
    /// out of the values the script receives.
    pub lint: Vec<LintFinding>,
}

/// Classify an error from the run into the account a draft reports. Anything that is not one of
/// the three run outcomes (a database failure, say) is not about the draft and stays an error.
fn draft_failure(error: AppError) -> Result<DraftRunFailure, AppError> {
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
#[utoipa::path(post, path = "/tool_scripts/draft_run", request_body = DraftRunRequest,
    responses((status = 200, body = DraftRunResponse,
               description = "The lint findings, plus results or the reason the run ended"),
              (status = 400, description = "Unreadable request body or manifest")),
    tag = "tool_scripts")]
pub async fn draft_run(
    State(state): State<AppState>,
    Json(payload): Json<DraftRunRequest>,
) -> AppResult<Json<DraftRunResponse>> {
    let mut draft_manifest = payload.manifest.clone();
    let mut manifest = engine::parse_manifest(&draft_manifest)
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
    let catalog = engine::check_manifest_against_catalog(
        &state.db,
        &mut manifest,
        Some(&mut draft_manifest),
        engine::MissingConstant::Omit,
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
    let content_hash = migration::tool_hash::stored_version_content(
        &state.db,
        &payload.script,
        &entry,
        &draft_manifest,
        &serde_json::json!({}),
    )
    .await?
    .content_hash;

    let tool = engine::ActiveTool::draft(payload.script, entry, manifest, content_hash);
    let body = serde_json::to_vec(&payload.inputs.unwrap_or_else(|| serde_json::json!({})))
        .unwrap_or_default();
    let outcome = engine::run_tool_body(
        &state,
        &tool,
        &body,
        payload.constants.as_ref(),
        engine::MissingConstant::Omit,
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
    let runtime = engine::runner_runtime(&state).await;
    Ok(Json(DraftRunResponse {
        ran: run.is_some(),
        run,
        failure,
        tool_version: tool.version_ref(runtime.as_ref()),
        lint,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct InspectScriptRequest {
    pub script: String,
    /// The function the runner would call. Defaults to `tool`.
    #[serde(default)]
    pub entry_function: Option<String>,
    /// A manifest to set the inspection against. Supplying one adds `reconciliation` to the
    /// response; nothing here reads or writes a stored manifest.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub manifest: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InspectScriptResponse {
    #[serde(flatten)]
    pub inspection: engine::ScriptInspection,
    /// Present only when the request carried a manifest.
    pub reconciliation: Option<engine::ManifestReconciliation>,
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
#[utoipa::path(post, path = "/tool_scripts/inspect", request_body = InspectScriptRequest,
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
            engine::parse_manifest(&raw)
                .map_err(|e| AppError::BadRequest(format!("invalid manifest: {e}")))
        })
        .transpose()?;
    let entry = payload.entry_function.as_deref().unwrap_or("tool");
    let inspection = engine::inspect_script(&state, &payload.script, entry).await?;
    let reconciliation = manifest
        .as_ref()
        .map(|m| engine::reconcile_manifest(&inspection, m));
    Ok(Json(InspectScriptResponse {
        inspection,
        reconciliation,
    }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VersionDetail {
    pub id: Uuid,
    pub version_no: i32,
    pub script: String,
    pub entry_function: String,
    #[schema(value_type = Object)]
    pub manifest: serde_json::Value,
    #[schema(value_type = Object)]
    pub test_cases: serde_json::Value,
    pub content_hash: String,
    pub note: Option<String>,
    pub created_by: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub validated_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn load_version(state: &AppState, script_id: Uuid, vid: Uuid) -> AppResult<VersionDetail> {
    let row = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id, version_no, script, entry_function, manifest, test_cases, content_hash,
                     note, created_by, created_at, validated_at
              FROM tool_script_versions WHERE id = $1 AND tool_script_id = $2",
            [vid.into(), script_id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound(format!("version {vid} not found")))?;
    Ok(VersionDetail {
        id: row.try_get("", "id")?,
        version_no: row.try_get("", "version_no")?,
        script: row.try_get("", "script")?,
        entry_function: row.try_get("", "entry_function")?,
        manifest: row.try_get("", "manifest")?,
        test_cases: row.try_get("", "test_cases")?,
        content_hash: row.try_get("", "content_hash")?,
        note: row.try_get("", "note")?,
        created_by: row.try_get("", "created_by")?,
        created_at: row.try_get("", "created_at")?,
        validated_at: row.try_get("", "validated_at")?,
    })
}

/// Full version content (script text, manifest, cases). Requires Administrator.
#[utoipa::path(get, path = "/tool_scripts/{id}/versions/{version_id}",
    params(("id" = Uuid, Path), ("version_id" = Uuid, Path)),
    responses((status = 200, body = VersionDetail)), tag = "tool_scripts")]
pub async fn get_version(
    State(state): State<AppState>,
    Path((id, vid)): Path<(Uuid, Uuid)>,
) -> AppResult<Json<VersionDetail>> {
    Ok(Json(load_version(&state, id, vid).await?))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CaseResult {
    pub name: String,
    pub passed: bool,
    /// Per-key mismatches: expected vs got, or the missing/unexpected key.
    pub failures: Vec<String>,
    /// The runner's error text when the script itself failed.
    pub error: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ValidateResponse {
    pub passed: bool,
    pub cases: Vec<CaseResult>,
    pub validated_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn as_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::Array(a) if a.len() == 1 => a[0].as_f64(),
        _ => None,
    }
}

/// Tolerant structural equality: numbers compare within `tol * max(|expected|, 1)` at any
/// nesting depth, and a result object may carry keys the expectation does not name.
fn matches_expected(got: &serde_json::Value, expected: &serde_json::Value, tol: f64) -> bool {
    if let Some(exp_n) = expected.as_f64() {
        return as_f64(got).is_some_and(|g| (g - exp_n).abs() <= tol * exp_n.abs().max(1.0));
    }
    match expected {
        serde_json::Value::Object(exp) => match got.as_object() {
            Some(g) => exp
                .iter()
                .all(|(k, v)| g.get(k).is_some_and(|gv| matches_expected(gv, v, tol))),
            None => false,
        },
        serde_json::Value::Array(exp) => match got.as_array() {
            Some(g) => {
                g.len() == exp.len()
                    && g.iter()
                        .zip(exp)
                        .all(|(gv, ev)| matches_expected(gv, ev, tol))
            }
            None => false,
        },
        other => got == other,
    }
}

/// The version under test, shaped as the calculate path sees a tool, so a case runs through the
/// same manifest handling: unknown fields, kind checks, defaults, requiredness and curve
/// resolution all apply, and a case that would be refused by `POST /tools/{name}/calculate` is
/// refused here.
fn version_as_tool(
    script: &ToolScriptSummary,
    version: &VersionDetail,
) -> AppResult<engine::ActiveTool> {
    let manifest = engine::parse_manifest(&version.manifest)
        .map_err(|e| AppError::BadRequest(format!("invalid manifest: {e}")))?;
    Ok(engine::ActiveTool {
        script_id: script.id,
        name: script.name.clone(),
        label: script.label.clone(),
        description: script.description.clone(),
        version_id: version.id,
        version_no: version.version_no,
        script: version.script.clone(),
        entry_function: version.entry_function.clone(),
        content_hash: version.content_hash.clone(),
        manifest,
    })
}

/// A case's request body: its inputs plus its curves, which the manifest names as fields of the
/// same body. A curve given as coefficients resolves without the database, which is what keeps a
/// case reproducible.
fn case_body(case: &serde_json::Value) -> serde_json::Value {
    let mut body = case
        .get("inputs")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    if let Some(curves) = case.get("curves").and_then(|v| v.as_object()) {
        for (name, curve) in curves {
            body.insert(name.clone(), curve.clone());
        }
    }
    serde_json::Value::Object(body)
}

/// Run a version's stored test cases through the runner, and record the outcome on the version.
///
/// The stamp follows the run in both directions. A version that passed once and fails now (a
/// constant retuned, a referenced curve edited) has its `validated_at` cleared, because the stamp
/// is what the activation gate reads and a gate that keeps yesterday's answer is not a gate.
async fn run_stored_cases(
    state: &AppState,
    id: Uuid,
    version: &VersionDetail,
) -> AppResult<ValidateResponse> {
    let tool = version_as_tool(&load_summary(state, id).await?, version)?;
    let tolerance = version.test_cases["tolerance"].as_f64().unwrap_or(1e-9);
    let empty = vec![];
    let cases = version.test_cases["cases"].as_array().unwrap_or(&empty);
    if cases.is_empty() {
        return Err(AppError::BadRequest(
            "this version has no test cases; add cases before validating".to_string(),
        ));
    }

    let mut results = Vec::with_capacity(cases.len());
    let mut all_passed = true;
    for (i, case) in cases.iter().enumerate() {
        let name = case["name"]
            .as_str()
            .map_or_else(|| format!("case {}", i + 1), str::to_string);
        // A case that names no constants falls back to the catalog, the same source the
        // calculate path reads.
        let constants = case.get("constants").and_then(|v| v.as_object());
        let body = serde_json::to_vec(&case_body(case)).unwrap_or_default();
        let outcome = engine::run_tool_body(
            state,
            &tool,
            &body,
            constants,
            engine::MissingConstant::Refuse,
        )
        .await;

        let mut failures = Vec::new();
        let mut error = None;
        match outcome {
            // The runner being absent says nothing about the cases, so it leaves the stamp alone
            // rather than recording a failure the script did not cause.
            Err(AppError::ServiceUnavailable(msg)) => {
                return Err(AppError::ServiceUnavailable(msg));
            }
            Err(e) => error = Some(e.to_string()),
            Ok(run) => {
                let got = &run.results;
                if let Some(expected) = case.get("expected").and_then(|e| e.as_object()) {
                    for (key, exp) in expected {
                        match got.get(key) {
                            Some(g) => {
                                if !matches_expected(g, exp, tolerance) {
                                    failures.push(format!("{key}: expected {exp}, got {g}"));
                                }
                            }
                            None => failures.push(format!("{key}: missing from result")),
                        }
                    }
                }
                if let Some(absent) = case.get("absent").and_then(|a| a.as_array()) {
                    for key in absent.iter().filter_map(|k| k.as_str()) {
                        if got.get(key).is_some_and(|v| !v.is_null()) {
                            failures.push(format!("{key}: expected absent, got {}", got[key]));
                        }
                    }
                }
            }
        }
        let passed = failures.is_empty() && error.is_none();
        all_passed &= passed;
        results.push(CaseResult {
            name,
            passed,
            failures,
            error,
        });
    }

    let validated_at = all_passed.then(chrono::Utc::now);
    state
        .db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE tool_script_versions SET validated_at = $2 WHERE id = $1",
            [version.id.into(), validated_at.into()],
        ))
        .await?;

    Ok(ValidateResponse {
        passed: all_passed,
        cases: results,
        validated_at,
    })
}

/// Run a version's stored test cases through the runner. All-pass stamps `validated_at`; a
/// failure clears it. Requires Administrator.
#[utoipa::path(post, path = "/tool_scripts/{id}/versions/{version_id}/validate",
    params(("id" = Uuid, Path), ("version_id" = Uuid, Path)),
    responses((status = 200, body = ValidateResponse)), tag = "tool_scripts")]
pub async fn validate_version(
    State(state): State<AppState>,
    Path((id, vid)): Path<(Uuid, Uuid)>,
) -> AppResult<Json<ValidateResponse>> {
    let version = load_version(&state, id, vid).await?;
    Ok(Json(run_stored_cases(&state, id, &version).await?))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ActivateRequest {}

#[derive(Debug, Serialize, ToSchema)]
pub struct ActivateResponse {
    #[serde(flatten)]
    pub script: ToolScriptSummary,
    /// What the manifest's catalog references amount to now, re-checked against the catalog as it
    /// stands rather than as it stood when the version was saved. A parameter deleted since then
    /// leaves a dead `parameter_id` behind, and this is where an operator can see it.
    pub lint: Vec<LintFinding>,
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
#[utoipa::path(post, path = "/tool_scripts/{id}/versions/{version_id}/activate",
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
    Json(_payload): Json<ActivateRequest>,
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
    let mut manifest = engine::parse_manifest(&version.manifest)
        .map_err(|e| AppError::BadRequest(format!("invalid manifest: {e}")))?;
    let catalog = engine::check_manifest_against_catalog(
        &state.db,
        &mut manifest,
        None,
        engine::MissingConstant::Refuse,
    )
    .await?;
    let lint: Vec<LintFinding> = catalog
        .errors
        .into_iter()
        .chain(catalog.warnings)
        .map(manifest_finding)
        .collect();
    let txn = state.db.begin().await?;
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"INSERT INTO tool_script_activations
              (tool_script_id, from_version_id, to_version_id, activated_by)
          SELECT id, active_version_id, $2, $3 FROM tool_scripts WHERE id = $1",
        [id.into(), vid.into(), actor_label(&auth).into()],
    ))
    .await?;
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE tool_scripts SET active_version_id = $2, updated_at = now() WHERE id = $1",
        [id.into(), vid.into()],
    ))
    .await?;
    txn.commit().await?;
    Ok(Json(ActivateResponse {
        script: load_summary(&state, id).await?,
        lint,
    }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ActivationRecord {
    pub from_version_no: Option<i32>,
    pub to_version_no: i32,
    pub activated_by: Option<String>,
    pub activated_at: chrono::DateTime<chrono::Utc>,
}

/// The script's activation history, newest first. Requires Administrator.
#[utoipa::path(get, path = "/tool_scripts/{id}/activations", params(("id" = Uuid, Path)),
    responses((status = 200, body = [ActivationRecord])), tag = "tool_scripts")]
pub async fn list_activations(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<ActivationRecord>>> {
    let rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT fv.version_no AS from_version_no, tv.version_no AS to_version_no,
                     a.activated_by, a.activated_at
              FROM tool_script_activations a
              JOIN tool_script_versions tv ON tv.id = a.to_version_id
              LEFT JOIN tool_script_versions fv ON fv.id = a.from_version_id
              WHERE a.tool_script_id = $1
              ORDER BY a.activated_at DESC",
            [id.into()],
        ))
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(ActivationRecord {
            from_version_no: row.try_get("", "from_version_no")?,
            to_version_no: row.try_get("", "to_version_no")?,
            activated_by: row.try_get("", "activated_by")?,
            activated_at: row.try_get("", "activated_at")?,
        });
    }
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::{
        FORBIDDEN, FUNCTION_ARGS, LIBRARY_WHITELIST, NAME_RESOLVERS, findings_from_scan,
        forbidden_reason,
    };
    use crate::routes::private::tools::engine::{ScannedArg, ScannedName, ScriptScan};

    fn scan(
        calls: Vec<(&str, i64)>,
        symbols: Vec<(&str, i64)>,
        args: Vec<ScannedArg>,
    ) -> ScriptScan {
        ScriptScan {
            parse_ok: true,
            parse_error: None,
            calls: calls
                .into_iter()
                .map(|(name, line)| ScannedName {
                    name: name.to_string(),
                    line,
                })
                .collect(),
            symbols: symbols
                .into_iter()
                .map(|(name, line)| ScannedName {
                    name: name.to_string(),
                    line,
                })
                .collect(),
            args,
        }
    }

    fn arg(call: &str, name: &str, value: &str, kind: &str, line: i64) -> ScannedArg {
        ScannedArg {
            call: call.to_string(),
            name: name.to_string(),
            value: value.to_string(),
            kind: kind.to_string(),
            line,
        }
    }

    /// R matches an argument name by prefix, so every prefix of `file` opens a file.
    #[test]
    fn an_abbreviated_file_argument_is_read_as_file() {
        for abbreviation in ["f", "fi", "fil", "file"] {
            let findings = findings_from_scan(&scan(
                vec![("cat", 4)],
                vec![],
                vec![arg("cat", abbreviation, "out.txt", "string", 4)],
            ));
            assert_eq!(findings.len(), 1, "{abbreviation}: {findings:?}");
            assert_eq!(findings[0].line, 4);
            assert!(findings[0].message.contains("'cat'"), "{findings:?}");
        }
        let sep_only = findings_from_scan(&scan(
            vec![("cat", 4)],
            vec![],
            vec![arg("cat", "sep", " ", "string", 4)],
        ));
        assert!(sep_only.is_empty(), "{sep_only:?}");
    }

    /// The same name reached three ways is the same finding, and each carries the line it was
    /// reached on.
    #[test]
    fn an_alias_and_a_resolved_name_are_reported_like_a_call() {
        let findings = findings_from_scan(&scan(
            vec![("system", 1), ("do.call", 3)],
            vec![("system", 2)],
            vec![arg("do.call", "", "system", "string", 3)],
        ));
        let at = |line: usize| -> Vec<&str> {
            findings
                .iter()
                .filter(|f| f.line == line)
                .map(|f| f.message.as_str())
                .collect()
        };
        for line in [1, 2, 3] {
            assert!(
                at(line).iter().any(|m| m.contains("'system'")),
                "line {line}: {findings:?}"
            );
        }
    }

    #[test]
    fn a_namespaced_call_is_read_as_its_package_and_its_function() {
        let internals = findings_from_scan(&scan(vec![("dplyr:::select", 1)], vec![], vec![]));
        assert!(
            internals.iter().any(|f| f.message.contains(":::")),
            "{internals:?}"
        );
        let outside =
            findings_from_scan(&scan(vec![("curl::curl_fetch_memory", 2)], vec![], vec![]));
        assert!(
            outside.iter().any(|f| f.message.contains("'curl'")),
            "{outside:?}"
        );
        let through_base = findings_from_scan(&scan(vec![("base::system", 3)], vec![], vec![]));
        assert!(
            through_base.iter().any(|f| f.message.contains("'system'")),
            "{through_base:?}"
        );
    }

    /// The resolvers are refused as calls in their own right, so a name built at run time is
    /// stopped where a string literal would have been read.
    #[test]
    fn every_name_resolver_is_forbidden_in_its_own_right() {
        for resolver in NAME_RESOLVERS {
            assert!(
                forbidden_reason(resolver).is_some(),
                "{resolver} resolves names but is allowed"
            );
        }
    }

    /// A namespaced name read in value position is the same three questions as a namespaced call:
    /// `runner <- base::system` reaches `system` and `x <- curl::handle` names a package that is
    /// not in the image.
    #[test]
    fn a_namespaced_name_is_read_the_same_called_or_assigned() {
        let assigned = findings_from_scan(&scan(vec![], vec![("base::system", 4)], vec![]));
        assert!(
            assigned.iter().any(|f| f.message.contains("'system'")),
            "{assigned:?}"
        );
        assert_eq!(assigned[0].line, 4);

        let outside = findings_from_scan(&scan(vec![], vec![("curl::handle", 2)], vec![]));
        assert!(
            outside.iter().any(|f| f.message.contains("'curl'")),
            "{outside:?}"
        );

        let internals = findings_from_scan(&scan(vec![], vec![("dplyr:::select", 6)], vec![]));
        assert!(
            internals.iter().any(|f| f.message.contains(":::")),
            "{internals:?}"
        );
    }

    /// `lapply(x, "system")` passes its string through `match.fun`, so the string is the call.
    #[test]
    fn a_function_named_as_a_string_is_read_as_that_function() {
        for call in FUNCTION_ARGS {
            let findings = findings_from_scan(&scan(
                vec![(call, 3)],
                vec![],
                vec![arg(call, "", "system", "string", 3)],
            ));
            assert!(
                findings.iter().any(|f| f.message.contains("'system'")),
                "{call}: {findings:?}"
            );
        }
        let ordinary = findings_from_scan(&scan(
            vec![("lapply", 3)],
            vec![],
            vec![arg("lapply", "", "mean", "string", 3)],
        ));
        assert!(ordinary.is_empty(), "{ordinary:?}");
    }

    #[test]
    fn no_whitelisted_package_shares_a_name_with_a_forbidden_construct() {
        for package in LIBRARY_WHITELIST {
            assert!(
                !FORBIDDEN.iter().any(|(name, _)| name == package),
                "{package} is both allowed and forbidden"
            );
        }
    }
}
