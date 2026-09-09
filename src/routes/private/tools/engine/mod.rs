//! Generic dispatch for DB-stored tool scripts.
//!
//! A tool is a row in `tool_scripts` whose active `tool_script_versions` row carries the R
//! script, its manifest (typed inputs, outputs, constants, curve slots) and its test cases.
//! Calculation resolves constants and curves here, so the runner receives values and the
//! provenance snapshot is taken where the data was read, then proxies to the OpenCPU runner.

use sea_orm::{ConnectionTrait, DatabaseConnection, FromQueryResult, Statement};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::readings::sd_estimator;
use crate::routes::private::sync::replicate_audit;

mod manifest;
mod runner;

pub use manifest::*;
pub use runner::*;

/// Which half of an output's declaration the catalog row was found by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedBy {
    Id,
    Code,
}

/// The catalog parameter an output is saved to, resolved server-side. Serving this is what lets a
/// caller stop matching strings against a page of the catalog it happens to have fetched.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ResolvedParameter {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    #[schema(required)]
    pub default_units: Option<String>,
    /// True for a catalog entry created mechanically rather than by a person.
    pub needs_review: bool,
    pub resolved_by: ResolvedBy,
    /// True when the output declares a `parameter_id` no catalog row holds and the code resolved
    /// instead. Resolution still lands on a parameter, so nothing breaks, but the authoritative
    /// half points at a deleted row and wants repair.
    pub dangling_parameter_id: bool,
}

#[derive(Debug, Clone, sea_orm::FromQueryResult)]
struct CatalogRow {
    id: Uuid,
    code: String,
    name: String,
    default_units: Option<String>,
    needs_review: bool,
}

impl CatalogRow {
    fn resolved(&self, resolved_by: ResolvedBy, dangling_parameter_id: bool) -> ResolvedParameter {
        ResolvedParameter {
            id: self.id,
            code: self.code.clone(),
            name: self.name.clone(),
            default_units: self.default_units.clone(),
            needs_review: self.needs_review,
            resolved_by,
            dangling_parameter_id,
        }
    }
}

/// The catalog rows a set of manifests can possibly name, read in one query and indexed both ways,
/// so resolving every output of every tool costs one round trip rather than one per output.
#[derive(Debug, Default)]
pub struct ParameterCatalog {
    by_id: std::collections::HashMap<Uuid, CatalogRow>,
    /// Keyed by lowercased code, matching the `LOWER(code)` unique index.
    by_code: std::collections::HashMap<String, CatalogRow>,
}

impl ParameterCatalog {
    /// A catalog of bare (id, code) rows, for tests that exercise resolution without a database.
    #[cfg(test)]
    pub(crate) fn with_codes(rows: &[(Uuid, &str)]) -> Self {
        let mut catalog = Self::default();
        for (id, code) in rows {
            let row = CatalogRow {
                id: *id,
                code: (*code).to_string(),
                name: (*code).to_string(),
                default_units: None,
                needs_review: false,
            };
            catalog.by_code.insert(code.to_lowercase(), row.clone());
            catalog.by_id.insert(*id, row);
        }
        catalog
    }

    /// The parameter an output names: `parameter_id` when it resolves, else
    /// `suggested_parameter_code`, else nothing.
    #[must_use]
    pub fn resolve(&self, output: &ManifestOutput) -> Option<ResolvedParameter> {
        if let Some(id) = output.parameter_id
            && let Some(row) = self.by_id.get(&id)
        {
            return Some(row.resolved(ResolvedBy::Id, false));
        }
        let code = output.suggested_parameter_code.as_ref()?;
        self.by_code
            .get(&code.to_lowercase())
            .map(|row| row.resolved(ResolvedBy::Code, output.parameter_id.is_some()))
    }

    fn row_by_id(&self, id: Uuid) -> Option<&CatalogRow> {
        self.by_id.get(&id)
    }

    /// The parameter a `replicates` param's `parameter_code` names.
    #[must_use]
    pub fn resolve_code(&self, code: &str) -> Option<ResolvedParameter> {
        self.by_code
            .get(&code.to_lowercase())
            .map(|row| row.resolved(ResolvedBy::Code, false))
    }
}

/// Read every catalog row the given manifests could name, by id or by code.
pub async fn load_parameter_catalog<'a>(
    db: &DatabaseConnection,
    manifests: impl IntoIterator<Item = &'a Manifest>,
) -> AppResult<ParameterCatalog> {
    let mut ids: Vec<Uuid> = Vec::new();
    let mut codes: Vec<String> = Vec::new();
    for manifest in manifests {
        for output in &manifest.outputs {
            if let Some(id) = output.parameter_id {
                ids.push(id);
            }
            if let Some(code) = &output.suggested_parameter_code {
                codes.push(code.to_lowercase());
            }
        }
        for param in &manifest.params {
            if let Some(code) = &param.parameter_code {
                codes.push(code.to_lowercase());
            }
        }
    }
    let mut catalog = ParameterCatalog::default();
    if ids.is_empty() && codes.is_empty() {
        return Ok(catalog);
    }
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, code, name, default_units, needs_review FROM parameters
              WHERE id = ANY($1) OR LOWER(code) = ANY($2)",
            [
                sea_orm::Value::Array(
                    sea_orm::sea_query::ArrayType::Uuid,
                    Some(Box::new(ids.into_iter().map(Into::into).collect())),
                ),
                codes.into(),
            ],
        ))
        .await?;
    for row in &rows {
        let entry = CatalogRow::from_query_result(row, "")?;
        catalog
            .by_code
            .insert(entry.code.to_lowercase(), entry.clone());
        catalog.by_id.insert(entry.id, entry);
    }
    Ok(catalog)
}

/// What a manifest's catalog references amount to when the version is saved.
///
/// An id that names no row is a refusal: it can only be a mistake, since nothing else could have
/// produced it. A code that names no row is reported instead, because an author may legitimately
/// declare an analyte before a manager creates it, and the seeded tools ship that way.
#[derive(Debug, Default)]
pub struct CatalogFindings {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Write the resolved code into the manifest JSON that is hashed, stored and served, so the
/// portable half of the declaration exists whatever the author sent.
fn stamp_code(raw: &mut serde_json::Value, index: usize, code: &str) {
    if let Some(output) = raw
        .get_mut("outputs")
        .and_then(|outputs| outputs.get_mut(index))
        .and_then(serde_json::Value::as_object_mut)
    {
        output.insert(
            "suggested_parameter_code".to_string(),
            serde_json::Value::String(code.to_string()),
        );
    }
}

/// Check a manifest's parameter and constant references against the catalog they will resolve
/// against at call time, and complete the parameter declarations that resolve.
///
/// An output that names an id and no code has the resolved code written into it, in `manifest` and
/// in `raw` when one is given: an id is meaningless in another database, so the half that travels
/// is stamped rather than left to the author to remember. An id and a code that name different
/// parameters are refused, since neither half can be preferred without guessing which analyte was
/// meant.
pub async fn check_manifest_against_catalog(
    db: &DatabaseConnection,
    manifest: &mut Manifest,
    mut raw: Option<&mut serde_json::Value>,
    missing_constant: MissingConstant,
) -> AppResult<CatalogFindings> {
    let mut findings = CatalogFindings::default();
    let catalog = load_parameter_catalog(db, std::iter::once(&*manifest)).await?;
    for (index, output) in manifest.outputs.iter_mut().enumerate() {
        if let Some(id) = output.parameter_id {
            match (catalog.row_by_id(id), &output.suggested_parameter_code) {
                (None, _) => findings.errors.push(format!(
                    "output '{}': parameter_id {id} is not in the parameter catalog",
                    output.key
                )),
                (Some(row), Some(code)) if !code.eq_ignore_ascii_case(&row.code) => {
                    findings.errors.push(format!(
                        "output '{}': parameter_id {id} is '{}' but suggested_parameter_code is \
                         '{code}'; an id and a code naming different parameters cannot both be \
                         what this output saves to",
                        output.key, row.code
                    ));
                }
                (Some(_), Some(_)) => {}
                (Some(row), None) => {
                    output.suggested_parameter_code = Some(row.code.clone());
                    if let Some(raw) = raw.as_deref_mut() {
                        stamp_code(raw, index, &row.code);
                    }
                }
            }
        }
        if let Some(code) = &output.suggested_parameter_code
            && catalog.resolve(output).is_none()
        {
            findings.warnings.push(format!(
                "output '{}': suggested_parameter_code '{code}' matches no parameter; \
                 saving this output needs a catalog entry",
                output.key
            ));
        }
    }
    for param in &manifest.params {
        if let Some(code) = &param.parameter_code
            && catalog.resolve_code(code).is_none()
        {
            findings.warnings.push(format!(
                "param '{}': parameter_code '{code}' matches no parameter; saving these \
                 replicates needs a catalog entry",
                param.name
            ));
        }
    }
    // The slot an output saves to is the resolved parameter, so a collision is on the id and not
    // on the code: one output can name an id and another the code of that same row.
    let mut claimed: std::collections::HashMap<Uuid, String> = std::collections::HashMap::new();
    for output in &manifest.outputs {
        let Some(resolved) = catalog.resolve(output) else {
            continue;
        };
        if let Some(first) = claimed.insert(resolved.id, output.key.clone()) {
            findings.errors.push(format!(
                "outputs '{first}' and '{}' both resolve to parameter '{}' ({}); two outputs \
                 saving to one parameter write two series into one slot",
                output.key, resolved.code, resolved.id
            ));
        }
    }
    findings.errors.extend(
        missing_constants(db, &manifest.constants)
            .await?
            .into_iter()
            .map(|name| match missing_constant {
                MissingConstant::Refuse => {
                    format!("constant '{name}' is not in the constants table")
                }
                MissingConstant::Omit => format!(
                    "constant '{name}' is not in the constants table, so it did not reach the \
                     script; check the spelling or create the constant"
                ),
            }),
    );
    Ok(findings)
}

/// The declared constant names the `constants` table does not hold.
async fn missing_constants(db: &DatabaseConnection, names: &[String]) -> AppResult<Vec<String>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT name FROM constants WHERE name = ANY($1)",
            [names.to_vec().into()],
        ))
        .await?;
    let mut present = Vec::with_capacity(rows.len());
    for row in &rows {
        present.push(row.try_get::<String>("", "name")?);
    }
    Ok(names
        .iter()
        .filter(|name| !present.contains(name))
        .cloned()
        .collect())
}

/// One manifest output as `GET /tools` serves it: the declaration as authored, plus the parameter
/// it resolves to now. `parameter` is null when the output names none, or names one this database
/// does not hold.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ToolOutput {
    #[serde(flatten)]
    pub declared: ManifestOutput,
    #[schema(required)]
    pub parameter: Option<ResolvedParameter>,
}

/// One tool as `GET /tools` lists it: the manifest plus the identity of the version serving it.
#[derive(Debug, Serialize, ToSchema)]
pub struct ToolDescriptor {
    pub name: String,
    pub label: String,
    #[schema(required)]
    pub description: Option<String>,
    pub endpoint: String,
    pub params: Vec<ManifestParam>,
    pub outputs: Vec<ToolOutput>,
    pub constants: Vec<String>,
    pub curves: Vec<ManifestCurve>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub site_inputs: Vec<ManifestSiteInput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub event_inputs: Vec<ManifestEventInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub qc: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<ManifestSection>,
    pub match_keywords: Vec<String>,
    pub script_version_id: Uuid,
    pub version_no: i32,
}

/// The exact code identity a result was produced by, recorded into the provenance blob. The
/// runtime fields are null when the runner did not answer `runtime_info`: a number is still
/// worth serving without them. The version fields are null for a draft run, where the content
/// that produced the number is not stored anywhere.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ToolVersionRef {
    #[schema(required)]
    pub script_version_id: Option<Uuid>,
    #[schema(required)]
    pub version_no: Option<i32>,
    pub content_hash: String,
    #[schema(required)]
    pub runner_image: Option<String>,
    #[schema(required)]
    pub r_version: Option<String>,
}

/// Which engine a calculation's versions carry (Q43). One concept, two ways of expressing the
/// arithmetic: R in the sandbox, or the formulas attached to the calculation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Script,
    Formula,
}

impl Engine {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "script" => Some(Self::Script),
            "formula" => Some(Self::Formula),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::Formula => "formula",
        }
    }
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
    pub engine: Engine,
    /// The parameter group this calculation reads and writes, when it declares one (M66).
    pub parameter_group_id: Option<Uuid>,
    /// The formula engine's formulas, in no particular order; empty for a script calculation.
    pub formulas: Vec<super::formula::PinnedFormula>,
}

fn stored_manifest(name: &str, raw: &serde_json::Value) -> AppResult<Manifest> {
    parse_manifest(raw)
        .map_err(|e| AppError::Internal(format!("tool '{name}' has an unreadable manifest: {e}")))
}

const ACTIVE_TOOL_SQL: &str = r"
    SELECT s.id AS script_id, s.name, s.label, s.description, s.engine, s.parameter_group_id,
           v.id AS version_id, v.version_no, v.script, v.entry_function, v.manifest,
           v.content_hash
    FROM tool_scripts s
    JOIN tool_script_versions v ON v.id = s.active_version_id";

/// One formula of a calculation as stored, before its `sources` blob is read into pairs.
#[derive(FromQueryResult)]
struct StoredFormula {
    tool_script_id: Uuid,
    sources: serde_json::Value,
    site_sources: serde_json::Value,
    code: String,
    name: String,
    units: Option<String>,
    formula: String,
    ordinal: i32,
    output_parameter_code: Option<String>,
    curve_slot: Option<String>,
    per_replicate: Option<String>,
}

/// A named constant as the runner receives it.
#[derive(FromQueryResult)]
struct StoredConstant {
    name: String,
    value: f64,
}

/// A site's name beside the whole row as jsonb, which is what a site source reads a property from.
#[derive(FromQueryResult)]
struct StoredSite {
    name: String,
    site: serde_json::Value,
}

/// The served spot value at one slot, which an event input resolves to. A NULL `value` is a slot
/// with nothing served at the instant, not a decode failure.
#[derive(FromQueryResult)]
struct ServedSpotValue {
    parameter_id: Uuid,
    value: Option<f64>,
}

/// A standard curve as the runner receives its coefficients.
#[derive(FromQueryResult)]
struct StoredCurve {
    slope: f64,
    intercept: f64,
    name: Option<String>,
}

/// [`ACTIVE_TOOL_SQL`]'s row. The manifest and the engine stay parses over it: a stored manifest
/// that no longer reads is a corrupt row, not a decode failure, and it says so by name.
#[derive(sea_orm::FromQueryResult)]
struct StoredActiveTool {
    script_id: Uuid,
    name: String,
    label: String,
    description: Option<String>,
    version_id: Uuid,
    version_no: i32,
    script: String,
    entry_function: String,
    content_hash: String,
    manifest: serde_json::Value,
    engine: String,
    parameter_group_id: Option<Uuid>,
}

fn row_to_active(row: &sea_orm::QueryResult) -> AppResult<ActiveTool> {
    let stored = StoredActiveTool::from_query_result(row, "")?;
    let manifest = stored_manifest(&stored.name, &stored.manifest)?;
    Ok(ActiveTool {
        script_id: stored.script_id,
        label: stored.label,
        description: stored.description,
        version_id: stored.version_id,
        version_no: stored.version_no,
        script: stored.script,
        entry_function: stored.entry_function,
        content_hash: stored.content_hash,
        manifest,
        engine: Engine::parse(&stored.engine).unwrap_or(Engine::Script),
        parameter_group_id: stored.parameter_group_id,
        formulas: Vec::new(),
        name: stored.name,
    })
}

/// A jsonb array of `[name, name]` pairs as the pairs themselves. Both source lists are built the
/// same way in SQL, so both are read the same way here.
fn name_pairs(raw: &serde_json::Value) -> Vec<(String, String)> {
    raw.as_array()
        .map(|pairs| {
            pairs
                .iter()
                .filter_map(|pair| {
                    Some((
                        pair.get(0)?.as_str()?.to_string(),
                        pair.get(1)?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The formulas attached to the given calculations, as `(script_id, formula)` pairs.
pub async fn load_formulas<C: ConnectionTrait>(
    db: &C,
    script_ids: &[Uuid],
) -> AppResult<Vec<(Uuid, super::formula::PinnedFormula)>> {
    if script_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT d.tool_script_id, d.code, d.name, NULLIF(d.units, '') AS units, d.formula,
                    d.ordinal, d.curve_slot, d.per_replicate,
                    out.code AS output_parameter_code,
                    COALESCE(
                        (SELECT jsonb_agg(jsonb_build_array(src.variable_name, p.code)
                                            ORDER BY src.variable_name)
                           FROM derived_parameter_sources src
                           JOIN parameters p ON p.id = src.parameter_id
                          WHERE src.derived_definition_id = d.id),
                        '[]'::jsonb) AS sources,
                    COALESCE(
                        (SELECT jsonb_agg(jsonb_build_array(src.variable_name, src.site_property)
                                            ORDER BY src.variable_name)
                           FROM derived_parameter_sources src
                          WHERE src.derived_definition_id = d.id
                            AND src.site_property IS NOT NULL),
                        '[]'::jsonb) AS site_sources
               FROM calculation_formulas d
               LEFT JOIN parameters out ON out.id = d.output_parameter_id
              WHERE d.tool_script_id = ANY($1)
              ORDER BY d.ordinal, d.code",
            [script_ids.to_vec().into()],
        ))
        .await?;
    let mut formulas = Vec::with_capacity(rows.len());
    for row in &rows {
        let stored = StoredFormula::from_query_result(row, "")?;
        let sources = name_pairs(&stored.sources);
        let site_sources = name_pairs(&stored.site_sources);
        formulas.push((
            stored.tool_script_id,
            super::formula::PinnedFormula {
                code: stored.code,
                label: stored.name,
                units: stored.units,
                formula: stored.formula,
                ordinal: stored.ordinal,
                output_parameter_code: stored.output_parameter_code,
                sources,
                site_sources,
                curve_slot: stored.curve_slot,
                per_replicate: stored.per_replicate,
            },
        ));
    }
    Ok(formulas)
}

/// Load the formulas of every formula calculation in the set. A script calculation is left alone.
async fn attach_formulas(db: &DatabaseConnection, tools: &mut [ActiveTool]) -> AppResult<()> {
    let ids: Vec<Uuid> = tools
        .iter()
        .filter(|t| t.engine == Engine::Formula)
        .map(|t| t.script_id)
        .collect();
    for (script_id, formula) in load_formulas(db, &ids).await? {
        if let Some(tool) = tools.iter_mut().find(|t| t.script_id == script_id) {
            tool.formulas.push(formula);
        }
    }
    Ok(())
}

/// The calculation set: every enabled tool with an active version. A disabled tool is left out
/// here, so the chain, the audit and the tools list do not see it; it can still be run by name.
pub async fn list_active_tools(db: &DatabaseConnection) -> AppResult<Vec<ActiveTool>> {
    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("{ACTIVE_TOOL_SQL} WHERE s.enabled ORDER BY s.name"),
        ))
        .await?;
    let mut tools: Vec<ActiveTool> = rows.iter().map(row_to_active).collect::<AppResult<_>>()?;
    attach_formulas(db, &mut tools).await?;
    Ok(tools)
}

pub async fn find_active_tool(db: &DatabaseConnection, name: &str) -> AppResult<ActiveTool> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!("{ACTIVE_TOOL_SQL} WHERE LOWER(s.name) = LOWER($1)"),
            [name.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Unknown tool: {name}")))?;
    let mut tools = vec![row_to_active(&row)?];
    attach_formulas(db, &mut tools).await?;
    Ok(tools.remove(0))
}

impl ActiveTool {
    /// A tool assembled from editor content that is not stored. Its ids are nil because no row
    /// carries this content, and `version_ref` reports that as an absent version identity, so a
    /// draft result cannot be saved as if a version had produced it.
    pub fn draft(
        script: String,
        entry_function: String,
        manifest: Manifest,
        content_hash: String,
    ) -> Self {
        Self {
            script_id: Uuid::nil(),
            name: "draft".to_string(),
            label: manifest.label.clone(),
            description: manifest.description.clone(),
            version_id: Uuid::nil(),
            version_no: 0,
            script,
            entry_function,
            content_hash,
            manifest,
            engine: Engine::Script,
            parameter_group_id: None,
            formulas: Vec::new(),
        }
    }

    pub fn descriptor(&self, catalog: &ParameterCatalog) -> ToolDescriptor {
        ToolDescriptor {
            name: self.name.clone(),
            label: self.manifest.label.clone(),
            description: self
                .manifest
                .description
                .clone()
                .or_else(|| self.description.clone()),
            endpoint: format!("/api/tools/{}/calculate", self.name),
            // A replicates param names the catalog parameter its values are stored as, so the
            // response carries the resolution the same way an output's does.
            params: self
                .manifest
                .params
                .iter()
                .map(|p| ManifestParam {
                    parameter: p
                        .parameter_code
                        .as_deref()
                        .and_then(|code| catalog.resolve_code(code)),
                    ..p.clone()
                })
                .collect(),
            outputs: self
                .manifest
                .outputs
                .iter()
                .map(|output| ToolOutput {
                    parameter: catalog.resolve(output),
                    declared: output.clone(),
                })
                .collect(),
            constants: self.manifest.constants.clone(),
            curves: self.manifest.curves.clone(),
            site_inputs: self.manifest.site_inputs.clone(),
            event_inputs: self.manifest.event_inputs.clone(),
            qc: self.manifest.qc.clone(),
            sections: self.manifest.sections.clone(),
            match_keywords: self.manifest.match_keywords.clone(),
            script_version_id: self.version_id,
            version_no: self.version_no,
        }
    }

    pub fn version_ref(&self, runtime: Option<&RunnerRuntime>) -> ToolVersionRef {
        ToolVersionRef {
            script_version_id: (!self.version_id.is_nil()).then_some(self.version_id),
            version_no: (!self.version_id.is_nil()).then_some(self.version_no),
            content_hash: self.content_hash.clone(),
            runner_image: runtime.and_then(|r| r.runner_image.clone()),
            r_version: runtime.and_then(|r| r.r_version.clone()),
        }
    }
}

/// A curve as the runner receives it: coefficients plus, when it came from the catalog, the
/// identity that resolves them.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ResolvedCurve {
    pub slope: f64,
    pub intercept: f64,
    #[schema(required)]
    pub standard_curve_id: Option<Uuid>,
    #[schema(required)]
    pub label: Option<String>,
}

/// One curve as the stored run records it: the slot it filled, and the curve itself.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CurveSnapshot {
    pub name: String,
    pub curve: ResolvedCurve,
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
            .query_one_raw(Statement::from_sql_and_values(
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
        let stored = StoredCurve::from_query_result(&row, "")?;
        return Ok(ResolvedCurve {
            slope: stored.slope,
            intercept: stored.intercept,
            standard_curve_id: Some(id),
            label: stored.name,
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

/// What a declared constant the `constants` table does not hold means for the run that declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingConstant {
    /// A stored version cannot be saved declaring a name the table does not hold, so an absence at
    /// call time is the catalog having lost a row.
    Refuse,
    /// Editor content: the name is as likely half-typed as deleted, and refusing the run would
    /// withhold both the numbers and the finding the author is writing against.
    Omit,
}

async fn resolve_constants(
    db: &DatabaseConnection,
    names: &[String],
    missing: MissingConstant,
) -> AppResult<serde_json::Map<String, serde_json::Value>> {
    let mut out = serde_json::Map::new();
    if names.is_empty() {
        return Ok(out);
    }
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT name, value FROM constants WHERE name = ANY($1)",
            [names.to_vec().into()],
        ))
        .await?;
    for row in &rows {
        let constant = StoredConstant::from_query_result(row, "")?;
        out.insert(constant.name, serde_json::json!(constant.value));
    }
    if missing == MissingConstant::Refuse {
        // A version cannot be saved declaring a constant that does not exist, so reaching here
        // means the row was deleted after the fact: the state of the catalog, not the request.
        for name in names {
            if !out.contains_key(name) {
                return Err(AppError::Conflict(format!(
                    "constant '{name}' is declared by this tool but no longer exists in the \
                     constants table; restore it or publish a version that does not declare it"
                )));
            }
        }
    }
    Ok(out)
}

pub struct RunOutcome {
    pub results: serde_json::Map<String, serde_json::Value>,
    /// Outputs the script computed as NA, which is a request to clear the stored value rather
    /// than an output it declined to name.
    pub cleared: Vec<String>,
    /// Formulas that did not run, as `{output, reason}`. An input a visit does not hold costs
    /// that one output and nothing else, so the run records which and why.
    pub skipped: Vec<serde_json::Value>,
    pub inputs_used: Vec<String>,
    pub inputs_ignored: Vec<String>,
    pub curves: Vec<CurveSnapshot>,
    pub constants: serde_json::Map<String, serde_json::Value>,
    /// The inputs exactly as the runner received them: request values, plus defaults and the
    /// resolved site/event inputs, minus the curves. This is what the stored run records, so a
    /// recompute replays what actually ran rather than what the client happened to type.
    pub inputs: serde_json::Map<String, serde_json::Value>,
    /// Station properties resolved from the site, as `{property, param, value}`.
    pub site_inputs: Vec<serde_json::Value>,
    /// Same-event parameter reads, as `{param, parameter_code, parameter_id, value}`.
    pub event_inputs: Vec<serde_json::Value>,
    /// The calculation context the request declared, echoed for the stored run.
    pub site_id: Option<Uuid>,
    pub collected_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Pop the reserved context fields off a request body. They are calculation context, not tool
/// inputs: every tool accepts them and none receives them.
fn take_context(
    body: &mut serde_json::Map<String, serde_json::Value>,
) -> AppResult<(Option<Uuid>, Option<chrono::DateTime<chrono::Utc>>)> {
    let site_id = match body.remove("site_id") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .and_then(|s| s.parse::<Uuid>().ok())
                .ok_or_else(|| AppError::BadRequest("site_id must be a UUID".to_string()))?,
        ),
    };
    let collected_at = match body.remove("collected_at") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&chrono::Utc))
                .ok_or_else(|| {
                    AppError::BadRequest("collected_at must be an RFC 3339 timestamp".to_string())
                })?,
        ),
    };
    Ok((site_id, collected_at))
}

/// Fill the params the manifest's `site_inputs` declare from the `sites` row, where the request
/// did not carry them. Any column of the row is resolvable (D13); a required property the site
/// does not hold refuses the run naming it.
pub async fn resolve_site_inputs(
    db: &DatabaseConnection,
    tool_name: &str,
    manifest: &Manifest,
    site_id: Option<Uuid>,
    body: &mut serde_json::Map<String, serde_json::Value>,
) -> AppResult<Vec<serde_json::Value>> {
    let pending: Vec<&ManifestSiteInput> = manifest
        .site_inputs
        .iter()
        .filter(|s| body.get(s.target()).is_none_or(serde_json::Value::is_null))
        .collect();
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    let Some(site_id) = site_id else {
        if let Some(required) = pending.iter().find(|s| s.required) {
            return Err(AppError::BadRequest(format!(
                "tool '{tool_name}' reads site property '{}'; pass site_id so it can be \
                 resolved, or supply '{}' directly",
                required.property,
                required.target()
            )));
        }
        return Ok(Vec::new());
    };
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT name, to_jsonb(s) AS site FROM sites s WHERE id = $1",
            [site_id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::BadRequest(format!("Site {site_id} not found")))?;
    let StoredSite {
        name: site_name,
        site,
    } = StoredSite::from_query_result(&row, "")?;

    let mut resolved = Vec::new();
    for s in pending {
        match site.get(&s.property) {
            Some(value) if !value.is_null() => {
                // The kind/structure checks ran before resolution, so a resolved value is
                // validated here or not at all: a text property filling a number param must be
                // refused, never handed to the runner.
                if let Some(param) = manifest.params.iter().find(|p| p.name == s.target())
                    && !kind_accepts(&param.kind, value)
                {
                    return Err(AppError::BadRequest(format!(
                        "site property '{}' of site '{site_name}' resolved to {value}, which                          is not a {} for input '{}' of tool '{tool_name}'",
                        s.property,
                        param.kind,
                        s.target()
                    )));
                }
                body.insert(s.target().to_string(), value.clone());
                resolved.push(serde_json::json!({
                    "property": s.property,
                    "param": s.target(),
                    "value": value,
                }));
            }
            _ if s.required => {
                return Err(AppError::BadRequest(format!(
                    "site '{site_name}' has no value for site property '{}', which tool \
                     '{tool_name}' requires",
                    s.property
                )));
            }
            _ => {}
        }
    }
    Ok(resolved)
}

/// The served spot value at one (site, parameter, instant): the sample mean, else the lowest
/// unflagged replicate that is not withdrawn. `$1` is the site and `$3` the instant; `parameter`
/// is the expression naming the parameter, so a statement resolving the parameter itself can
/// pass its own column instead of a placeholder.
#[must_use]
pub fn served_spot_value_sql(parameter: &str) -> String {
    format!(
        "COALESCE(
            (SELECT smp.mean FROM samples smp
              WHERE smp.site_id = $1 AND smp.parameter_id = {parameter}
                AND smp.collected_at = $3),
            (SELECT COALESCE(r.calibrated_value, r.raw_value) FROM readings r
              WHERE r.site_id = $1 AND r.parameter_id = {parameter} AND r.time = $3
                AND r.measurement_type = 'spot' AND r.is_flagged IS NOT TRUE
                AND r.withdrawn_at IS NULL
              ORDER BY r.replicate_index LIMIT 1)
         )"
    )
}

/// Fill the params the manifest's `event_inputs` declare from the collection event's stored
/// readings, where the request did not carry them. The value is the served spot value: the sample
/// mean, else the lowest unflagged replicate. Absence is not an error here — the param's own
/// requiredness decides whether the run can proceed without it.
pub async fn resolve_event_inputs(
    db: &DatabaseConnection,
    tool_name: &str,
    manifest: &Manifest,
    site_id: Option<Uuid>,
    collected_at: Option<chrono::DateTime<chrono::Utc>>,
    body: &mut serde_json::Map<String, serde_json::Value>,
) -> AppResult<Vec<serde_json::Value>> {
    let pending: Vec<&ManifestEventInput> = manifest
        .event_inputs
        .iter()
        .filter(|e| body.get(&e.param).is_none_or(serde_json::Value::is_null))
        .collect();
    let (Some(site_id), Some(collected_at)) = (site_id, collected_at) else {
        return Ok(Vec::new());
    };
    let mut resolved = Vec::new();
    for e in pending {
        let Some(row) = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                &format!(
                    "SELECT p.id AS parameter_id, {} AS value
                     FROM parameters p WHERE LOWER(p.code) = LOWER($2)",
                    served_spot_value_sql("p.id")
                ),
                [
                    site_id.into(),
                    e.parameter_code.clone().into(),
                    sea_orm::prelude::DateTimeWithTimeZone::from(collected_at).into(),
                ],
            ))
            .await?
        else {
            continue;
        };
        let served = ServedSpotValue::from_query_result(&row, "")?;
        let parameter_id = served.parameter_id;
        let Some(value) = served.value else {
            continue;
        };
        if let Some(param) = manifest.params.iter().find(|p| p.name == e.param)
            && !kind_accepts(&param.kind, &serde_json::json!(value))
        {
            return Err(AppError::BadRequest(format!(
                "event input '{}' resolved to the number {value}, which is not a {} for input                  '{}' of tool '{tool_name}'",
                e.parameter_code, param.kind, e.param
            )));
        }
        body.insert(e.param.clone(), serde_json::json!(value));
        resolved.push(serde_json::json!({
            "param": e.param,
            "parameter_code": e.parameter_code,
            "parameter_id": parameter_id,
            "value": value,
        }));
    }
    Ok(resolved)
}

/// Validate a request body against the tool's manifest, resolve its constants and curves, and
/// execute the script in the runner.
pub async fn run_active_tool(
    state: &AppState,
    tool: &ActiveTool,
    body: &[u8],
) -> AppResult<RunOutcome> {
    run_tool_body(state, tool, body, None, MissingConstant::Refuse).await
}

/// The same path as [`run_active_tool`], with the option of taking constant values from the
/// caller instead of the catalog. A stored test case carries its own constants so it stays
/// reproducible whatever the `constants` table holds, and validation still has to exercise the
/// manifest handling the calculate path applies.
pub async fn run_tool_body(
    state: &AppState,
    tool: &ActiveTool,
    body: &[u8],
    constants_override: Option<&serde_json::Map<String, serde_json::Value>>,
    missing_constant: MissingConstant,
) -> AppResult<RunOutcome> {
    let resolved = resolve_run(state, tool, body, constants_override, missing_constant).await?;
    execute_resolved(state, tool, resolved).await
}

/// A run resolved up to the point of execution: exactly what the runner will receive, before it
/// is asked for anything.
pub struct ResolvedRun {
    /// The inputs as the runner will receive them: request values plus defaults and the resolved
    /// site/event inputs, minus the curves.
    pub inputs: serde_json::Map<String, serde_json::Value>,
    pub constants: serde_json::Map<String, serde_json::Value>,
    /// Curves by slot name, as the runner receives them.
    pub curves: serde_json::Map<String, serde_json::Value>,
    /// The same curves as `{name, curve}` snapshots, the form the stored run records.
    pub curve_snapshots: Vec<CurveSnapshot>,
    curves_consumed: Vec<String>,
    provided: Vec<String>,
    pub site_inputs: Vec<serde_json::Value>,
    pub event_inputs: Vec<serde_json::Value>,
    pub site_id: Option<Uuid>,
    pub collected_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl ResolvedRun {
    /// The identity of what this run would consume under `version_id`. Equal fingerprints mean
    /// equal outputs, so a recompute can skip the runner.
    pub fn fingerprint(&self, version_id: Uuid) -> String {
        run_fingerprint(
            version_id,
            &serde_json::Value::Object(self.inputs.clone()),
            &serde_json::Value::Object(self.constants.clone()),
            &serde_json::to_value(&self.curve_snapshots).unwrap_or_default(),
        )
    }
}

/// One hash over a script version and the inputs, constants and curve snapshots a run consumes,
/// in the canonical form the stored run and provenance blob hold. Computed the same way from a
/// resolved run and from a stored record, so the two are comparable.
pub fn run_fingerprint(
    version_id: Uuid,
    inputs: &serde_json::Value,
    constants: &serde_json::Value,
    curves: &serde_json::Value,
) -> String {
    crate::routes::private::tools::hash::canonical_hash(&serde_json::json!({
        "script_version_id": version_id,
        "inputs": inputs,
        "constants": constants,
        "curves": curves,
    }))
}

/// What a request body must be before anything is resolved from it: every key is a param or a
/// curve the manifest declares, and every value present is of the kind and the structure its param
/// declares. Nothing here reads the database or the runner, so the contract a caller meets is
/// testable without either.
///
/// Requiredness is not checked here: a resolved site or event input fills a gap after this runs,
/// so a field absent at this point may still be supplied.
pub fn check_body_shape(
    tool_name: &str,
    manifest: &Manifest,
    body: &serde_json::Map<String, serde_json::Value>,
) -> AppResult<()> {
    let param_names: Vec<&str> = manifest.params.iter().map(|p| p.name.as_str()).collect();
    let curve_names: Vec<&str> = manifest.curves.iter().map(|c| c.name.as_str()).collect();

    for key in body.keys() {
        if !param_names.contains(&key.as_str()) && !curve_names.contains(&key.as_str()) {
            return Err(AppError::BadRequest(format!(
                "unknown field '{key}' for tool '{tool_name}'"
            )));
        }
    }
    for p in &manifest.params {
        let Some(value) = body.get(&p.name).filter(|v| !v.is_null()) else {
            continue;
        };
        if !kind_accepts(&p.kind, value) {
            return Err(AppError::BadRequest(format!(
                "Invalid request body: field '{}' must be {} for tool '{tool_name}'",
                p.name, p.kind
            )));
        }
        if let Some(structure) = &p.structure {
            structure.check_value(&p.name, value).map_err(|e| {
                AppError::BadRequest(format!("Invalid request body: {e} for tool '{tool_name}'"))
            })?;
        }
    }
    Ok(())
}

/// Everything [`run_tool_body`] does before the runner is called: the manifest checks, the
/// context resolution, defaults and requiredness, curve and constant resolution.
pub async fn resolve_run(
    state: &AppState,
    tool: &ActiveTool,
    body: &[u8],
    constants_override: Option<&serde_json::Map<String, serde_json::Value>>,
    missing_constant: MissingConstant,
) -> AppResult<ResolvedRun> {
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

    let (site_id, collected_at) = take_context(&mut body)?;

    let manifest = &tool.manifest;
    check_body_shape(&tool.name, manifest, &body)?;
    // Resolved context values land before defaults and requiredness: a typed value wins, a
    // resolved one fills the gap, and a manifest default is the last resort.
    let site_inputs =
        resolve_site_inputs(&state.db, &tool.name, manifest, site_id, &mut body).await?;
    let event_inputs = resolve_event_inputs(
        &state.db,
        &tool.name,
        manifest,
        site_id,
        collected_at,
        &mut body,
    )
    .await?;

    // Defaults land before requiredness so a condition reads the same values the runner will,
    // whatever order the params are declared in.
    for p in &manifest.params {
        let present = body.get(&p.name).is_some_and(|v| !v.is_null());
        if !present && let Some(default) = &p.default {
            body.insert(p.name.clone(), default.clone());
        }
    }
    for p in &manifest.params {
        if body.get(&p.name).is_some_and(|v| !v.is_null()) || !p.required {
            continue;
        }
        let enforced = match &p.when {
            None => true,
            Some(ParamWhen::Condition(c)) => c.holds(&body),
            Some(ParamWhen::Note(_)) => false,
        };
        if enforced {
            return Err(AppError::BadRequest(format!(
                "missing required field '{}' for tool '{}'",
                p.name, tool.name
            )));
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
                curve_snapshots.push(CurveSnapshot {
                    name: slot.name.clone(),
                    curve: resolved,
                });
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

    let constants = match constants_override {
        Some(supplied) => {
            let mut out = serde_json::Map::new();
            for name in &manifest.constants {
                let value = supplied.get(name).ok_or_else(|| {
                    AppError::BadRequest(format!(
                        "constant '{name}' is declared by the manifest but not supplied"
                    ))
                })?;
                out.insert(name.clone(), value.clone());
            }
            out
        }
        None => resolve_constants(&state.db, &manifest.constants, missing_constant).await?,
    };
    let provided: Vec<String> = body.keys().cloned().collect();
    Ok(ResolvedRun {
        inputs: body,
        constants,
        curves,
        curve_snapshots,
        curves_consumed,
        provided,
        site_inputs,
        event_inputs,
        site_id,
        collected_at,
    })
}

/// Hand a resolved run to the runner and shape its answer: NA outputs dropped, manifest
/// aggregates applied, `inputs_used` accounted.
pub async fn execute_resolved(
    state: &AppState,
    tool: &ActiveTool,
    resolved: ResolvedRun,
) -> AppResult<RunOutcome> {
    let manifest = &tool.manifest;
    let param_names: Vec<&str> = manifest.params.iter().map(|p| p.name.as_str()).collect();
    let ResolvedRun {
        inputs: effective_inputs,
        constants,
        curves,
        curve_snapshots,
        curves_consumed,
        provided,
        site_inputs,
        event_inputs,
        site_id,
        collected_at,
    } = resolved;

    // The engine decides only how the arithmetic is done. Everything after this point, the
    // cleared outputs, the manifest aggregates, the inputs the run records, is one path.
    let mut skipped: Vec<serde_json::Value> = Vec::new();
    let raw = if tool.engine == Engine::Formula {
        let numbers: std::collections::HashMap<String, f64> = effective_inputs
            .iter()
            .filter_map(|(k, v)| v.as_f64().map(|n| (k.clone(), n)))
            .collect();
        let constant_values: std::collections::HashMap<String, f64> = constants
            .iter()
            .filter_map(|(k, v)| v.as_f64().map(|n| (k.clone(), n)))
            .collect();
        let curve_values: std::collections::HashMap<String, super::formula::Curve> = curves
            .iter()
            .filter_map(|(name, curve)| {
                let slope = curve.get("slope")?.as_f64()?;
                let intercept = curve.get("intercept")?.as_f64()?;
                Some((name.clone(), super::formula::Curve { slope, intercept }))
            })
            .collect();
        // A replicate-shaped input arrives as an array, one entry per index with `null` for a
        // repeat not measured. It is what a per-replicate formula runs over, so it is carried
        // separately rather than dropped by the scalar filter above.
        let replicate_values: std::collections::HashMap<String, Vec<Option<f64>>> =
            effective_inputs
                .iter()
                .filter_map(|(k, v)| {
                    let items = v.as_array()?;
                    Some((
                        k.clone(),
                        items.iter().map(serde_json::Value::as_f64).collect(),
                    ))
                })
                .collect();
        let produced = super::formula::evaluate_over_replicates(
            &tool.formulas,
            &numbers,
            &replicate_values,
            &constant_values,
            &curve_values,
        )
        .map_err(|message| AppError::ToolScriptError {
            message: format!("{}: {message}", tool.name),
            call: None,
            traceback: Vec::new(),
        })?;
        let mut results = serde_json::Map::new();
        for entry in produced {
            match entry {
                super::formula::Produced::Scalar(evaluated) => {
                    if let Some(reason) = evaluated.skipped {
                        skipped.push(
                            serde_json::json!({ "output": evaluated.code, "reason": reason }),
                        );
                        continue;
                    }
                    // A value the formula computed as NA is an explicit null, which clears the
                    // stored value; a skipped formula names no output at all.
                    results.insert(evaluated.code, serde_json::json!(evaluated.value));
                }
                // One value per replicate index, gaps kept in place, which is the shape the save
                // path already verifies a reading's value against leaf by leaf.
                super::formula::Produced::PerReplicate { code, values, .. } => {
                    results.insert(code, serde_json::json!(values));
                }
            }
        }
        serde_json::Value::Object(results)
    } else {
        execute_script(
            state,
            &tool.script,
            &tool.entry_function,
            &serde_json::Value::Object(effective_inputs.clone()),
            &serde_json::Value::Object(constants.clone()),
            &serde_json::Value::Object(curves),
        )
        .await?
    };

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
    let cleared = partition_cleared(&mut results);

    apply_manifest_aggregates(
        &state.db,
        manifest,
        &effective_inputs,
        &curve_snapshots,
        site_id,
        collected_at,
        &mut results,
    )
    .await?;

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
        cleared,
        skipped,
        inputs_used,
        inputs_ignored,
        curves: curve_snapshots,
        constants,
        inputs: effective_inputs,
        site_inputs,
        event_inputs,
        site_id,
        collected_at,
    })
}

/// Split the script's result map into the values it produced and the outputs it cleared.
///
/// An NA arrives as null. The portal wrote NULL into that column rather than leaving the old
/// number standing, so a null is a clear, not an omission: the key leaves `results`, where every
/// consumer reads a value, and travels as `cleared`, which the chain turns into a withdrawal of
/// the stored output. An output the script never named is not in the map at all and is untouched.
fn partition_cleared(results: &mut serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let mut cleared: Vec<String> = results
        .iter()
        .filter(|(_, v)| v.is_null())
        .map(|(k, _)| k.clone())
        .collect();
    cleared.sort();
    results.retain(|_, v| !v.is_null());
    cleared
}

/// Compute the manifest's `aggregate` outputs over the curve-applied replicate values, replacing
/// anything the script emitted under the same keys. The preview must be the number the database
/// will serve after the save: same curve, same divisor. The divisor is the output's fixed
/// declaration, else what [`displayed_sd_estimator`] resolves for the instant being calculated.
async fn apply_manifest_aggregates(
    db: &DatabaseConnection,
    manifest: &Manifest,
    inputs: &serde_json::Map<String, serde_json::Value>,
    curve_snapshots: &[CurveSnapshot],
    site_id: Option<Uuid>,
    collected_at: Option<chrono::DateTime<chrono::Utc>>,
    results: &mut serde_json::Map<String, serde_json::Value>,
) -> AppResult<()> {
    for output in &manifest.outputs {
        let (Some(kind), Some(source)) =
            (output.aggregate.as_deref(), output.aggregate_of.as_deref())
        else {
            continue;
        };
        let Some(param) = manifest.params.iter().find(|p| p.name == source) else {
            continue;
        };
        let Some(cells) = inputs.get(source).and_then(serde_json::Value::as_array) else {
            results.remove(&output.key);
            continue;
        };

        // The same linear application storage performs; a slot the request did not fill leaves
        // the values raw, exactly as the save would.
        let (slope, intercept) = param
            .curve
            .as_deref()
            .and_then(|slot| {
                curve_snapshots
                    .iter()
                    .find(|s| s.name == slot)
                    .map(|s| (s.curve.slope, s.curve.intercept))
            })
            .unwrap_or((1.0, 0.0));
        let values: Vec<f64> = cells
            .iter()
            .filter_map(serde_json::Value::as_f64)
            .map(|v| v * slope + intercept)
            .collect();

        let computed = match kind {
            "mean" => replicate_audit::group_stats(&values).mean,
            "sd" => {
                let estimator = match output.fixed_sd_estimator() {
                    Some(fixed) => fixed,
                    None => displayed_sd_estimator(db, site_id, collected_at, param)
                        .await?
                        .unwrap_or(sd_estimator::SAMPLE),
                };
                replicate_audit::group_stats(&values).under(estimator).sd
            }
            _ => None,
        };
        match computed {
            Some(v) => {
                results.insert(output.key.clone(), serde_json::json!(v));
            }
            None => {
                results.remove(&output.key);
            }
        }
    }
    Ok(())
}

/// The divisor the database will serve for the group this run is calculating, reachable only when
/// the run carries a site and the param names a catalog code.
///
/// Same ladder as the write path (`sd_estimator::resolve`), preceded by the one declaration that
/// belongs to the instant rather than the slot: an audit resolution scoped to a collection group
/// records its choice on that `samples` row, and the trigger computes the served sd from it. A
/// display resolved from the slot alone would show a different standard deviation for the same
/// values.
async fn displayed_sd_estimator(
    db: &DatabaseConnection,
    site_id: Option<Uuid>,
    collected_at: Option<chrono::DateTime<chrono::Utc>>,
    param: &ManifestParam,
) -> AppResult<Option<&'static str>> {
    let (Some(site), Some(code)) = (site_id, param.parameter_code.as_deref()) else {
        return Ok(None);
    };
    let Some(parameter_id) = catalog_parameter_id(db, code).await? else {
        return Ok(None);
    };
    if let Some(at) = collected_at
        && let Some(estimator) =
            sd_estimator::instant_declaration(db, site, parameter_id, at).await?
    {
        return Ok(Some(estimator));
    }
    let resolved = sd_estimator::resolve(db, site, parameter_id, None, None).await?;
    Ok(resolved.is_declared().then_some(resolved.estimator))
}

/// The catalog parameter a manifest param names, matched the way every other code lookup does.
async fn catalog_parameter_id(db: &DatabaseConnection, code: &str) -> AppResult<Option<Uuid>> {
    let row = db
        .query_one_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM parameters WHERE LOWER(code) = LOWER($1)",
            [code.into()],
        ))
        .await?;
    Ok(row.map(|r| r.try_get::<Uuid>("", "id")).transpose()?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_na_output_is_a_clear_and_an_unnamed_output_is_left_alone() {
        use super::partition_cleared;
        let mut results = serde_json::json!({
            "doc_avg": 1.25, "doc_sd": null, "dom": null, "flag": "ok"
        })
        .as_object()
        .unwrap()
        .clone();
        let cleared = partition_cleared(&mut results);
        assert_eq!(cleared, vec!["doc_sd".to_string(), "dom".to_string()]);
        assert_eq!(
            serde_json::Value::Object(results),
            serde_json::json!({ "doc_avg": 1.25, "flag": "ok" }),
            "a cleared key never reaches a consumer reading values"
        );
        // A run that named nothing clears nothing: silence is not a request to blank a column.
        let mut empty = serde_json::Map::new();
        assert!(partition_cleared(&mut empty).is_empty());
    }

    use super::{Manifest, ParamWhen, StructLayout};

    /// The request-body contract: what `/tools/{name}/calculate` refuses before it resolves
    /// anything. This was reached only through a Keycloak login, a database and the R runner.
    mod body_shape {
        use super::super::check_body_shape;
        use crate::routes::private::tools::engine::Manifest;

        fn doc_manifest() -> Manifest {
            serde_json::from_value(serde_json::json!({
                "label": "DOC",
                "params": [
                    { "name": "DOC", "label": "DOC", "kind": "replicates", "parameter_code": "DOC" },
                    { "name": "dilution", "label": "Dilution", "kind": "number" },
                    { "name": "operator", "label": "Operator", "kind": "string" },
                ],
                "curves": [{ "name": "std_curve", "label": "Standard curve" }],
                "outputs": [{ "key": "doc_avg", "label": "DOC average" }],
            }))
            .expect("the manifest parses")
        }

        fn check(body: serde_json::Value) -> Result<(), String> {
            let serde_json::Value::Object(map) = body else {
                panic!("the body is an object");
            };
            check_body_shape("doc", &doc_manifest(), &map).map_err(|e| e.to_string())
        }

        #[test]
        fn a_key_the_manifest_declares_nothing_for_is_refused_naming_it() {
            let err = check(serde_json::json!({ "DOC": [1.0], "typo": 1 }))
                .expect_err("an undeclared key is refused");
            assert!(err.contains("typo"), "{err}");
            assert!(err.contains("doc"), "{err}");
        }

        #[test]
        fn a_curve_slot_is_a_declared_key_like_a_param() {
            check(serde_json::json!({ "DOC": [1.0], "std_curve": "c1" }))
                .expect("a declared curve slot is accepted");
        }

        #[test]
        fn a_value_of_the_wrong_kind_is_refused_naming_the_field_and_the_kind() {
            let err = check(serde_json::json!({ "DOC": ["not-a-number"] }))
                .expect_err("a text cell in a numeric replicate list is refused");
            assert!(err.contains("DOC"), "{err}");

            let err = check(serde_json::json!({ "dilution": "two" }))
                .expect_err("text in a number param is refused");
            assert!(err.contains("dilution") && err.contains("number"), "{err}");
        }

        #[test]
        fn an_absent_or_null_field_passes_the_shape_check() {
            // Requiredness runs after the resolvers, which may still fill the gap.
            check(serde_json::json!({})).expect("an empty body has no shape error");
            check(serde_json::json!({ "DOC": serde_json::Value::Null }))
                .expect("an explicit null is an absence, not a wrong kind");
        }

        #[test]
        fn a_replicates_list_may_be_gapped_and_of_any_length() {
            check(serde_json::json!({ "DOC": [1.0, null, 3.0, 4.0, 5.0, 6.0] }))
                .expect("the operator chooses the count and a null is a repeat not measured");
        }
    }

    fn manifest_with(param: serde_json::Value) -> Result<Manifest, serde_json::Error> {
        serde_json::from_value(serde_json::json!({ "label": "T", "params": [param] }))
    }

    fn manifest(raw: serde_json::Value) -> Result<Manifest, serde_json::Error> {
        serde_json::from_value(raw)
    }

    fn doc_replicates(extra: serde_json::Value) -> serde_json::Value {
        let mut param = serde_json::json!({
            "name": "DOC", "label": "DOC", "kind": "replicates", "units": "ppb",
            "parameter_code": "DOC"
        });
        if let Some(map) = extra.as_object() {
            param.as_object_mut().unwrap().extend(map.clone());
        }
        param
    }

    #[test]
    fn a_replicates_param_takes_a_gapped_list_of_any_length() {
        let m = manifest(serde_json::json!({
            "label": "DOC",
            "params": [doc_replicates(serde_json::json!({ "suggested": 3, "curve": "std_curve" }))],
            "curves": [{ "name": "std_curve", "label": "Curve" }]
        }))
        .unwrap();
        let p = &m.params[0];
        assert_eq!(p.parameter_code.as_deref(), Some("DOC"));
        assert_eq!(p.suggested, Some(3));
        assert_eq!(p.curve.as_deref(), Some("std_curve"));
        for value in [
            serde_json::json!([120.0]),
            serde_json::json!([120.0, null, 118.0]),
            serde_json::json!([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
            serde_json::json!([]),
        ] {
            assert!(super::kind_accepts(&p.kind, &value), "{value}");
        }
        for value in [
            serde_json::json!(120.0),
            serde_json::json!(["120"]),
            serde_json::json!({ "0": 120.0 }),
        ] {
            assert!(!super::kind_accepts(&p.kind, &value), "{value}");
        }
    }

    #[test]
    fn a_replicates_param_that_is_not_anchored_is_refused() {
        for (raw, expected) in [
            (
                serde_json::json!({
                    "label": "T",
                    "params": [{ "name": "DOC", "label": "DOC", "kind": "replicates" }]
                }),
                "must name the parameter_code",
            ),
            (
                serde_json::json!({
                    "label": "T",
                    "params": [doc_replicates(serde_json::json!({ "suggested": 0 }))]
                }),
                "at least 1",
            ),
            (
                serde_json::json!({
                    "label": "T",
                    "params": [doc_replicates(serde_json::json!({ "curve": "nope" }))]
                }),
                "names no curve slot",
            ),
            (
                serde_json::json!({
                    "label": "T",
                    "params": [{ "name": "a", "label": "A", "kind": "number", "parameter_code": "DOC" }]
                }),
                "belong to a replicates param",
            ),
        ] {
            let err = manifest(raw).unwrap_err().to_string();
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn a_section_must_be_declared_once_and_named_by_key() {
        let m = manifest(serde_json::json!({
            "label": "T",
            "sections": [{ "key": "lab", "label": "Lab" }],
            "params": [doc_replicates(serde_json::json!({ "section": "lab" }))]
        }))
        .unwrap();
        assert_eq!(m.params[0].section.as_deref(), Some("lab"));
        assert_eq!(m.sections[0].key, "lab");
        for (raw, expected) in [
            (
                serde_json::json!({
                    "label": "T",
                    "params": [doc_replicates(serde_json::json!({ "section": "lab" }))]
                }),
                "not declared",
            ),
            (
                serde_json::json!({
                    "label": "T",
                    "sections": [{ "key": "lab", "label": "Lab" }, { "key": "lab", "label": "Lab 2" }]
                }),
                "declared twice",
            ),
        ] {
            let err = manifest(raw).unwrap_err().to_string();
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn a_manifest_without_the_new_fields_serializes_as_it_was_read() {
        let raw = serde_json::json!({
            "label": "T",
            "params": [{ "name": "a", "label": "A", "kind": "number", "units": null,
                         "required": false, "default": null, "when": null }]
        });
        let m = manifest(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(&m.params).unwrap(), raw["params"]);
    }

    #[test]
    fn a_curve_description_is_kept() {
        let m = manifest(serde_json::json!({
            "label": "T",
            "curves": [{ "name": "c", "label": "C", "description": "y = ax + b" }]
        }))
        .unwrap();
        assert_eq!(m.curves[0].description.as_deref(), Some("y = ax + b"));
        let plain = manifest(serde_json::json!({
            "label": "T",
            "curves": [{ "name": "c", "label": "C" }]
        }))
        .unwrap();
        let json = serde_json::to_value(&plain.curves).unwrap();
        assert!(json[0].get("description").is_none());
    }

    fn grid(structure: serde_json::Value) -> Result<Manifest, serde_json::Error> {
        manifest_with(serde_json::json!({
            "name": "replicates", "label": "Replicates", "kind": "replicate_grid",
            "structure": structure
        }))
    }

    #[test]
    fn a_declaration_fills_the_defaults_its_layout_implies() {
        let manifest = grid(serde_json::json!({
            "fields": [{ "name": "vol_ml", "label": "Vol", "units": "mL" }]
        }))
        .unwrap();
        let structure = manifest.params[0].structure.as_ref().unwrap();
        assert_eq!(structure.layout, StructLayout::Rows);
        assert_eq!(structure.rows, 3);
        assert_eq!(structure.fields[0].values, 1);
        assert!(structure.fields[0].send);
    }

    #[test]
    fn a_declaration_that_contradicts_its_param_is_refused() {
        for (param, expected) in [
            (
                serde_json::json!({ "name": "n", "label": "N", "kind": "number",
                    "structure": { "fields": [{ "name": "a", "label": "A" }] } }),
                "object or replicate_grid",
            ),
            (
                serde_json::json!({ "name": "o", "label": "O", "kind": "object",
                    "structure": { "layout": "rows", "fields": [{ "name": "a", "label": "A" }] } }),
                "does not fit kind",
            ),
        ] {
            let err = manifest_with(param).unwrap_err().to_string();
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn a_computed_field_must_name_fields_of_its_own_structure() {
        let err = grid(serde_json::json!({
            "fields": [
                { "name": "dried_g", "label": "Dried", "send": false },
                { "name": "afdm_g", "label": "AFDM",
                  "computed": { "subtract": ["dried_g", "ashed_g"] } }
            ]
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("ashed_g"), "{err}");
    }

    #[test]
    fn a_value_is_checked_against_the_columns_the_structure_declares() {
        let manifest = grid(serde_json::json!({
            "fields": [
                { "name": "vol_ml", "label": "Vol" },
                { "name": "diameters_cm", "label": "Diameters", "values": 3 },
                { "name": "dried_g", "label": "Dried", "send": false }
            ]
        }))
        .unwrap();
        let structure = manifest.params[0].structure.as_ref().unwrap();
        let check = |rows: serde_json::Value| structure.check_value("replicates", &rows);

        assert!(check(serde_json::json!([{ "vol_ml": 1.0, "diameters_cm": [1.0, null] }])).is_ok());
        // A blank row is what an untouched replicate looks like.
        assert!(check(serde_json::json!([{}])).is_ok());

        for (value, expected) in [
            (serde_json::json!([{ "nope": 1.0 }]), "declares no 'nope'"),
            (serde_json::json!([{ "dried_g": 1.0 }]), "entry-only"),
            (serde_json::json!([{ "vol_ml": [1.0] }]), "must be a number"),
            (
                serde_json::json!([{ "diameters_cm": 1.0 }]),
                "must be a list of numbers",
            ),
            (serde_json::json!([1.0]), "must be an object"),
        ] {
            let err = check(value).unwrap_err();
            assert!(err.contains(expected), "{err}");
            assert!(err.contains("replicates"), "{err}");
        }
    }

    #[test]
    fn an_open_structure_takes_a_column_it_does_not_declare() {
        let manifest = manifest_with(serde_json::json!({
            "name": "species", "label": "Species", "kind": "object",
            "structure": {
                "layout": "lists", "values": 3, "additional_fields": true,
                "fields": [{ "name": "NOx", "label": "NOx" }]
            }
        }))
        .unwrap();
        let structure = manifest.params[0].structure.as_ref().unwrap();
        assert!(
            structure
                .check_value("species", &serde_json::json!({ "TDN": [1.0, null, 3.0] }))
                .is_ok()
        );
        let err = structure
            .check_value("species", &serde_json::json!({ "TDN": 1.0 }))
            .unwrap_err();
        assert!(err.contains("list of numbers"), "{err}");
    }

    #[test]
    fn a_misspelled_manifest_key_is_refused_naming_it() {
        use super::parse_manifest;
        let err = parse_manifest(&serde_json::json!({
            "label": "T",
            "site_input": [{ "property": "altitude_m" }]
        }))
        .unwrap_err();
        assert!(err.contains("site_input"), "{err}");

        let err = parse_manifest(&serde_json::json!({
            "label": "T",
            "params": [{ "name": "t", "label": "T", "kind": "number", "requried": true }]
        }))
        .unwrap_err();
        assert!(err.contains("requried"), "{err}");
        assert!(err.contains("params"), "{err}");

        let err = parse_manifest(&serde_json::json!({
            "label": "T",
            "outputs": [{ "key": "doc", "label": "DOC", "agregate": "mean" }]
        }))
        .unwrap_err();
        assert!(err.contains("agregate"), "{err}");

        let err = parse_manifest(&serde_json::json!({
            "label": "T",
            "params": [
                { "name": "mode", "label": "Mode", "kind": "string" },
                { "name": "t", "label": "T", "kind": "number",
                  "when": { "param": "mode", "equal": "full" } }
            ]
        }))
        .unwrap_err();
        assert!(err.contains("equal"), "{err}");
    }

    #[test]
    fn the_station_inputs_spelling_of_site_inputs_still_parses() {
        let m = manifest(serde_json::json!({
            "label": "T",
            "params": [{ "name": "altitude_m", "label": "Altitude", "kind": "number" }],
            "station_inputs": [{ "property": "altitude_m" }]
        }))
        .unwrap();
        assert_eq!(m.site_inputs.len(), 1);
        assert_eq!(m.site_inputs[0].target(), "altitude_m");
    }

    #[test]
    fn a_manifest_kind_outside_the_vocabulary_is_refused() {
        let raw = serde_json::json!({
            "label": "T",
            "params": [{ "name": "hue", "label": "Hue", "kind": "colour" }]
        });
        let err = serde_json::from_value::<Manifest>(raw)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown kind 'colour'"), "{err}");
    }

    #[test]
    fn an_aggregate_outside_mean_and_sd_is_refused() {
        let raw = serde_json::json!({
            "label": "T",
            "params": [{ "name": "reps", "label": "Reps", "kind": "replicates",
                         "parameter_code": "X" }],
            "outputs": [{ "key": "med", "label": "Median", "aggregate_of": "reps",
                          "aggregate": "median" }]
        });
        let err = serde_json::from_value::<Manifest>(raw)
            .unwrap_err()
            .to_string();
        assert!(err.contains("'median' is not 'mean' or 'sd'"), "{err}");
    }

    #[test]
    fn an_aggregate_without_a_source_is_refused() {
        let raw = serde_json::json!({
            "label": "T",
            "outputs": [{ "key": "avg", "label": "Avg", "aggregate": "mean" }]
        });
        let err = serde_json::from_value::<Manifest>(raw)
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs aggregate_of"), "{err}");
    }

    #[test]
    fn an_aggregate_over_a_non_replicates_param_is_refused() {
        let raw = serde_json::json!({
            "label": "T",
            "params": [{ "name": "temp", "label": "Temp", "kind": "number" }],
            "outputs": [{ "key": "avg", "label": "Avg", "aggregate_of": "temp",
                          "aggregate": "mean" }]
        });
        let err = serde_json::from_value::<Manifest>(raw)
            .unwrap_err()
            .to_string();
        assert!(err.contains("names no replicates param"), "{err}");
    }

    #[test]
    fn a_display_marker_without_aggregate_stays_free_form() {
        let raw = serde_json::json!({
            "label": "T",
            "params": [{ "name": "x", "label": "X", "kind": "number" }],
            "outputs": [{ "key": "x_avg", "label": "Avg", "aggregate_of": "x_family" }]
        });
        assert!(serde_json::from_value::<Manifest>(raw).is_ok());
    }

    #[test]
    fn a_free_text_when_stays_a_note_and_an_object_becomes_a_condition() {
        let raw = serde_json::json!({
            "label": "T",
            "params": [
                { "name": "mode", "label": "Mode", "kind": "string" },
                { "name": "a", "label": "A", "kind": "number", "required": true,
                  "when": "mode=full" },
                { "name": "b", "label": "B", "kind": "number", "required": true,
                  "when": { "param": "mode", "equals": "full" } }
            ]
        });
        let manifest: Manifest = serde_json::from_value(raw).unwrap();
        assert!(matches!(manifest.params[1].when, Some(ParamWhen::Note(_))));
        let Some(ParamWhen::Condition(cond)) = &manifest.params[2].when else {
            panic!("the object form parses as a condition");
        };
        let mut body = serde_json::Map::new();
        assert!(!cond.holds(&body));
        body.insert("mode".into(), serde_json::json!("full"));
        assert!(cond.holds(&body));
        body.insert("mode".into(), serde_json::json!("simple"));
        assert!(!cond.holds(&body));
    }

    /// The entity is a site; `station_inputs` is what every stored manifest was written with.
    #[test]
    fn both_spellings_of_the_site_inputs_key_parse_the_same() {
        let by_site = manifest(serde_json::json!({
            "label": "T",
            "params": [{ "name": "alt", "label": "Altitude", "kind": "number" }],
            "site_inputs": [{ "property": "altitude_m", "param": "alt" }],
        }))
        .expect("site_inputs parses");
        let by_station = manifest(serde_json::json!({
            "label": "T",
            "params": [{ "name": "alt", "label": "Altitude", "kind": "number" }],
            "station_inputs": [{ "property": "altitude_m", "param": "alt" }],
        }))
        .expect("station_inputs still parses");

        assert_eq!(by_site.site_inputs.len(), 1);
        assert_eq!(by_station.site_inputs.len(), 1);
        assert_eq!(
            by_site.site_inputs[0].property,
            by_station.site_inputs[0].property
        );
        assert_eq!(by_site.site_inputs[0].target(), "alt");
        assert_eq!(by_station.site_inputs[0].target(), "alt");
    }

    /// The chain executor and the event-input resolver read the same instant, so both render
    /// this one string. The predicates below are the spot serving contract.
    #[test]
    fn test_served_spot_value_sql_carries_the_serving_predicates() {
        use super::served_spot_value_sql;
        for parameter in ["$2", "p.id"] {
            let sql = served_spot_value_sql(parameter);
            assert!(sql.contains("SELECT smp.mean FROM samples smp"));
            assert!(sql.contains("r.measurement_type = 'spot'"));
            assert!(sql.contains("r.is_flagged IS NOT TRUE"));
            assert!(sql.contains("r.withdrawn_at IS NULL"));
            assert!(sql.contains("ORDER BY r.replicate_index LIMIT 1"));
            assert_eq!(
                sql.matches(&format!("parameter_id = {parameter}")).count(),
                2
            );
        }
    }

    #[test]
    fn test_served_spot_value_sql_differs_only_in_the_parameter_expression() {
        use super::served_spot_value_sql;
        assert_eq!(
            served_spot_value_sql("p.id"),
            served_spot_value_sql("$2").replace("$2", "p.id")
        );
    }
}
