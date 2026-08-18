//! Admin authoring surface for DB-stored tool scripts.
//!
//! Versions are immutable: creating one appends, activation flips the script's pointer (recorded
//! in `tool_script_activations`), and rollback is activating an older version. A version can only
//! activate after its stored test cases pass against the runner.

use axum::{
    Json,
    extract::{Path, State},
};
use sea_orm::{ConnectionTrait, Statement, TransactionTrait};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::engine;
use crate::common::AppState;
use crate::error::{AppError, AppResult};

/// Packages a script may load beyond the pre-attached dplyr/tidyr/magrittr. Mirrors the runner
/// image's installed set; anything else fails at run time anyway, the lint just says so earlier.
const LIBRARY_WHITELIST: &[&str] = &[
    "dplyr", "tidyr", "magrittr", "pracma", "signal", "bigleaf", "stats", "utils", "methods",
];

/// Constructs whose only uses in a tool script are accidents. The runner container is the real
/// boundary; this scan turns the accident into an editor error with a line number.
const FORBIDDEN: &[(&str, &str)] = &[
    ("system(", "shell execution"),
    ("system2(", "shell execution"),
    ("shell(", "shell execution"),
    ("pipe(", "shell execution"),
    ("socketConnection(", "network access"),
    ("download.file(", "network access"),
    ("url(", "network access"),
    ("curl::", "network access"),
    ("install.packages(", "package installation"),
    ("Sys.setenv(", "environment mutation"),
    ("file.remove(", "file deletion"),
    ("unlink(", "file deletion"),
    (".Internal(", "internal calls"),
    (".Call(", "native calls"),
    ("quit(", "session control"),
    ("eval(parse(", "dynamic evaluation"),
];

#[derive(Debug, Serialize, ToSchema)]
pub struct LintFinding {
    pub line: usize,
    pub message: String,
}

fn lint_script(script: &str) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    for (idx, line) in script.lines().enumerate() {
        let code = line.split('#').next().unwrap_or("");
        for (token, why) in FORBIDDEN {
            if code.contains(token) {
                findings.push(LintFinding {
                    line: idx + 1,
                    message: format!(
                        "'{}' is not allowed in tool scripts ({why})",
                        token.trim_end_matches('(')
                    ),
                });
            }
        }
        for call in ["library(", "require("] {
            if let Some(pos) = code.find(call) {
                let arg: String = code[pos + call.len()..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '.' || *c == '_')
                    .collect();
                if !arg.is_empty() && !LIBRARY_WHITELIST.contains(&arg.as_str()) {
                    findings.push(LintFinding {
                        line: idx + 1,
                        message: format!("package '{arg}' is not in the runner image"),
                    });
                }
            }
        }
    }
    findings
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
            r"SELECT id, version_no, content_hash, entry_function, created_by, created_at,
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
    #[serde(default)]
    pub created_by: Option<String>,
}

/// Create a tool script (no versions yet; it lists in `GET /tools` only once a version is
/// activated). Requires Administrator.
#[utoipa::path(post, path = "/tool_scripts", request_body = CreateScriptRequest,
    responses((status = 200, body = ToolScriptSummary)), tag = "tool_scripts")]
pub async fn create_script(
    State(state): State<AppState>,
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
                payload.created_by.into(),
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
    #[serde(default)]
    pub created_by: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CreateVersionResponse {
    pub version: VersionSummary,
    pub lint: Vec<LintFinding>,
}

/// Append an immutable version. Refused when the lint finds forbidden constructs, the manifest
/// does not parse, or an identical version already exists. Requires Administrator.
#[utoipa::path(post, path = "/tool_scripts/{id}/versions", params(("id" = Uuid, Path)),
    request_body = CreateVersionRequest,
    responses((status = 200, body = CreateVersionResponse),
              (status = 400, description = "Lint findings or invalid manifest")),
    tag = "tool_scripts")]
pub async fn create_version(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<CreateVersionRequest>,
) -> AppResult<Json<CreateVersionResponse>> {
    load_summary(&state, id).await?;

    let findings = lint_script(&payload.script);
    if !findings.is_empty() {
        return Err(AppError::ConflictDetail {
            message: "the script did not pass the safety lint".to_string(),
            detail: serde_json::to_value(&findings).unwrap_or_default(),
        });
    }
    let manifest: engine::Manifest = serde_json::from_value(payload.manifest.clone())
        .map_err(|e| AppError::BadRequest(format!("invalid manifest: {e}")))?;
    drop(manifest);

    let entry = payload.entry_function.unwrap_or_else(|| "tool".to_string());
    let row = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"INSERT INTO tool_script_versions
                  (tool_script_id, version_no, script, entry_function, manifest, test_cases,
                   content_hash, created_by)
              SELECT $1,
                     COALESCE((SELECT max(version_no) FROM tool_script_versions
                               WHERE tool_script_id = $1), 0) + 1,
                     $2, $3, $4::jsonb, COALESCE($5::jsonb, '{}'::jsonb), md5($2), $6
              RETURNING id",
            [
                id.into(),
                payload.script.into(),
                entry.into(),
                serde_json::to_string(&payload.manifest)
                    .unwrap_or_default()
                    .into(),
                payload
                    .test_cases
                    .map(|c| serde_json::to_string(&c).unwrap_or_default())
                    .into(),
                payload.created_by.into(),
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
        lint: vec![],
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
                     created_by, created_at, validated_at
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

/// Run a version's stored test cases through the runner. All-pass stamps `validated_at`, the
/// activation prerequisite. Requires Administrator.
#[utoipa::path(post, path = "/tool_scripts/{id}/versions/{version_id}/validate",
    params(("id" = Uuid, Path), ("version_id" = Uuid, Path)),
    responses((status = 200, body = ValidateResponse)), tag = "tool_scripts")]
pub async fn validate_version(
    State(state): State<AppState>,
    Path((id, vid)): Path<(Uuid, Uuid)>,
) -> AppResult<Json<ValidateResponse>> {
    let version = load_version(&state, id, vid).await?;
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
        let outcome = engine::execute_script(
            &state,
            &version.script,
            &version.entry_function,
            case.get("inputs").unwrap_or(&serde_json::json!({})),
            case.get("constants").unwrap_or(&serde_json::json!({})),
            case.get("curves").unwrap_or(&serde_json::json!({})),
        )
        .await;

        let mut failures = Vec::new();
        let mut error = None;
        match outcome {
            Err(AppError::ServiceUnavailable(msg)) => {
                return Err(AppError::ServiceUnavailable(msg));
            }
            Err(e) => error = Some(e.to_string()),
            Ok(got) => {
                let empty_map = serde_json::Map::new();
                let got = got.as_object().unwrap_or(&empty_map);
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

    let validated_at = if all_passed {
        let now = chrono::Utc::now();
        state
            .db
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE tool_script_versions SET validated_at = $2 WHERE id = $1",
                [vid.into(), now.into()],
            ))
            .await?;
        Some(now)
    } else {
        None
    };

    Ok(Json(ValidateResponse {
        passed: all_passed,
        cases: results,
        validated_at,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ActivateRequest {
    #[serde(default)]
    pub activated_by: Option<String>,
}

/// Make a version the one `GET /tools` serves. Refused until validated. Activating an older
/// version is the rollback; every flip lands in the activation audit. Requires Administrator.
#[utoipa::path(post, path = "/tool_scripts/{id}/versions/{version_id}/activate",
    params(("id" = Uuid, Path), ("version_id" = Uuid, Path)),
    request_body = ActivateRequest,
    responses((status = 200, body = ToolScriptSummary),
              (status = 409, description = "Version not validated")),
    tag = "tool_scripts")]
pub async fn activate_version(
    State(state): State<AppState>,
    Path((id, vid)): Path<(Uuid, Uuid)>,
    Json(payload): Json<ActivateRequest>,
) -> AppResult<Json<ToolScriptSummary>> {
    let version = load_version(&state, id, vid).await?;
    if version.validated_at.is_none() {
        return Err(AppError::Conflict(
            "this version has not passed validation; run validate first".to_string(),
        ));
    }
    let txn = state.db.begin().await?;
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"INSERT INTO tool_script_activations
              (tool_script_id, from_version_id, to_version_id, activated_by)
          SELECT id, active_version_id, $2, $3 FROM tool_scripts WHERE id = $1",
        [id.into(), vid.into(), payload.activated_by.into()],
    ))
    .await?;
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE tool_scripts SET active_version_id = $2, updated_at = now() WHERE id = $1",
        [id.into(), vid.into()],
    ))
    .await?;
    txn.commit().await?;
    Ok(Json(load_summary(&state, id).await?))
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
