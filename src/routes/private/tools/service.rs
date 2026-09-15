//! Tool queries and the pure logic they share: the manifest catalog check, formula
//! evaluation, the closure walk, script linting and the runner calls.

use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::sea_query::{Alias, Expr, Func, JoinType, Order, PostgresQueryBuilder, Query};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, EntityTrait,
    FromQueryResult, QueryFilter, QueryOrder, QuerySelect, Set, Statement, TransactionTrait,
};
use sea_orm_migration::sea_orm::DbErr;

use crate::routes::private::readings::models as readings;
use crate::routes::private::readings::samples::models as samples;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use uuid::Uuid;

use super::flows::dependency_order;
use super::models::script::{self, ToolScript};
use super::models::version as version_entity;
use super::models::version::{ToolScriptVersion, ToolScriptVersionList};
use super::models::{
    ActiveTool, CalculationHealth, CalculationImpact, CaseResult, CatalogFindings, ClosureQuery,
    Curve, CurveSnapshot, Engine, Evaluated, ImpactParameter, LintFinding, Manifest, ManifestCurve,
    ManifestEventInput, ManifestOutput, ManifestParam, ManifestSiteInput, MissingConstant,
    ParamWhen, ParseCheck, ParseError, PinnedFormula, Produced, ResolvedBy, ResolvedCurve,
    ResolvedParameter, RunOutcome, RunnerRuntime, ScannedName, ScriptInspection, ScriptScan,
    SlotCoverage, StoredVersionContent, Subject, ToolScriptOperations, TraceCell, TraceStep,
    ValidateResponse, kind_accepts, parse_manifest,
};
use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::constants::models as constants;
use crate::routes::private::data_streams::models as data_streams;
use crate::routes::private::parameter_groups::service::rules;
use crate::routes::private::parameters::models as parameters;
use crate::routes::private::sensor_calibrations::service::evaluate_formula;
use crate::routes::private::site_parameters::models as site_parameters;
use crate::routes::private::sites::models as sites;
use crate::routes::private::standard_curves::models as standard_curves;
use crate::routes::private::sync::hold_model;
use crate::routes::private::sync::models::{HoldKind, HoldStatus};
use crate::routes::private::sync::service as replicate_audit;

#[derive(Debug, Clone, sea_orm::FromQueryResult)]
pub(super) struct CatalogRow {
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
    let rows = parameters::Entity::find()
        .filter(
            sea_orm::Condition::any()
                .add(parameters::Column::Id.is_in(ids))
                .add(lowered_code_in(&codes)),
        )
        .all(db)
        .await?;
    for row in rows {
        let entry = CatalogRow {
            id: row.id,
            code: row.code,
            name: row.name,
            default_units: Some(row.default_units),
            needs_review: row.needs_review,
        };
        catalog
            .by_code
            .insert(entry.code.to_lowercase(), entry.clone());
        catalog.by_id.insert(entry.id, entry);
    }
    Ok(catalog)
}

/// Write the resolved code into the manifest JSON that is hashed, stored and served, so the
/// portable half of the declaration exists whatever the author sent.
pub(super) fn stamp_code(raw: &mut serde_json::Value, index: usize, code: &str) {
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
pub(super) async fn missing_constants(
    db: &DatabaseConnection,
    names: &[String],
) -> AppResult<Vec<String>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let present: Vec<String> = constants::Entity::find()
        .select_only()
        .column(constants::Column::Name)
        .filter(constants::Column::Name.is_in(names.to_vec()))
        .into_tuple()
        .all(db)
        .await?;
    Ok(names
        .iter()
        .filter(|name| !present.contains(name))
        .cloned()
        .collect())
}

pub(super) fn stored_manifest(name: &str, raw: &serde_json::Value) -> AppResult<Manifest> {
    parse_manifest(raw)
        .map_err(|e| AppError::Internal(format!("tool '{name}' has an unreadable manifest: {e}")))
}

pub(super) const ACTIVE_TOOL_SQL: &str = r"
    SELECT s.id AS script_id, s.name, s.label, s.description, s.engine, s.parameter_group_id,
           v.id AS version_id, v.version_no, v.script, v.entry_function, v.manifest,
           v.content_hash
    FROM tool_scripts s
    JOIN tool_script_versions v ON v.id = s.active_version_id";

/// One formula of a calculation as stored, before its `sources` blob is read into pairs.
#[derive(FromQueryResult)]
pub(super) struct StoredFormula {
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
    intermediate: bool,
}

/// The served spot value at one slot, which an event input resolves to. A NULL `value` is a slot
/// with nothing served at the instant, not a decode failure.
#[derive(FromQueryResult)]
pub(super) struct ServedSpotValue {
    value: Option<f64>,
}

/// [`ACTIVE_TOOL_SQL`]'s row. The manifest and the engine stay parses over it: a stored manifest
/// that no longer reads is a corrupt row, not a decode failure, and it says so by name.
#[derive(sea_orm::FromQueryResult)]
pub(super) struct StoredActiveTool {
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

pub(super) fn row_to_active(row: &sea_orm::QueryResult) -> AppResult<ActiveTool> {
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
pub(super) fn name_pairs(raw: &serde_json::Value) -> Vec<(String, String)> {
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

/// The formulas each calculation runs: the ones it owns, plus the shared steps it declares
/// (Q156). A declared step is evaluated in every run that reads it, under its own code, because a
/// single run has no other run's value to take.
pub async fn load_formulas<C: ConnectionTrait>(
    db: &C,
    script_ids: &[Uuid],
) -> AppResult<Vec<(Uuid, PinnedFormula)>> {
    let mut formulas = load_own_formulas(db, script_ids).await?;
    formulas.extend(load_declared_steps(db, script_ids).await?);
    Ok(formulas)
}

/// A step's `(variable, parameter code)` readings and its `(variable, site column)` readings, the
/// two shapes a `PinnedFormula` carries them in.
type StepSources = (Vec<(String, String)>, Vec<(String, String)>);

/// The steps the given calculations declare, as the same pairs their own formulas arrive in. One
/// step declared by two calculations is one pair each.
async fn load_declared_steps<C: ConnectionTrait>(
    db: &C,
    script_ids: &[Uuid],
) -> AppResult<Vec<(Uuid, PinnedFormula)>> {
    use crate::routes::private::derived_parameters::models::{definition, shared_step, source};

    if script_ids.is_empty() {
        return Ok(Vec::new());
    }
    let declarations = shared_step::Entity::find()
        .filter(shared_step::Column::ToolScriptId.is_in(script_ids.to_vec()))
        .all(db)
        .await?;
    if declarations.is_empty() {
        return Ok(Vec::new());
    }
    let formula_ids: Vec<Uuid> = declarations.iter().map(|d| d.formula_id).collect();
    let steps = definition::Entity::find()
        .filter(definition::Column::Id.is_in(formula_ids.clone()))
        .all(db)
        .await?;
    let sources = source::Entity::find()
        .filter(source::Column::DerivedDefinitionId.is_in(formula_ids))
        .find_also_related(crate::routes::private::parameters::Entity)
        .all(db)
        .await?;

    let mut by_formula: HashMap<Uuid, StepSources> = HashMap::new();
    for (row, parameter) in sources {
        let entry = by_formula.entry(row.derived_definition_id).or_default();
        if let Some(parameter) = parameter {
            entry.0.push((row.variable_name.clone(), parameter.code));
        }
        if let Some(column) = row.site_property {
            entry.1.push((row.variable_name, column));
        }
    }
    for pairs in by_formula.values_mut() {
        pairs.0.sort();
        pairs.1.sort();
    }

    let mut pairs = Vec::with_capacity(declarations.len());
    for declaration in declarations {
        let Some(step) = steps.iter().find(|s| s.id == declaration.formula_id) else {
            continue;
        };
        let (sources, site_sources) = by_formula
            .get(&step.id)
            .cloned()
            .unwrap_or_else(|| (Vec::new(), Vec::new()));
        pairs.push((
            declaration.tool_script_id,
            PinnedFormula {
                code: step.code.clone(),
                label: step.name.clone(),
                units: Some(step.units.clone()).filter(|u| !u.is_empty()),
                formula: step.formula.clone(),
                ordinal: step.ordinal,
                output_parameter_code: None,
                sources,
                site_sources,
                curve_slot: step.curve_slot.clone(),
                per_replicate: step.per_replicate.clone(),
                intermediate: step.intermediate,
            },
        ));
    }
    Ok(pairs)
}

/// The formulas the given calculations own, as `(script_id, formula)` pairs.
async fn load_own_formulas<C: ConnectionTrait>(
    db: &C,
    script_ids: &[Uuid],
) -> AppResult<Vec<(Uuid, PinnedFormula)>> {
    if script_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT d.tool_script_id, d.code, d.name, NULLIF(d.units, '') AS units, d.formula,
                    d.ordinal, d.curve_slot, d.per_replicate, d.intermediate,
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
            PinnedFormula {
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
                intermediate: stored.intermediate,
            },
        ));
    }
    Ok(formulas)
}

/// Load the formulas of every formula calculation in the set. A script calculation is left alone.
pub(super) async fn attach_formulas(
    db: &DatabaseConnection,
    tools: &mut [ActiveTool],
) -> AppResult<()> {
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

pub(super) async fn resolve_curve(
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
        let stored = standard_curves::Entity::find_by_id(id)
            .one(db)
            .await?
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "curve '{}': standard curve {id} not found",
                    slot.name
                ))
            })?;
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

pub(super) async fn resolve_constants(
    db: &DatabaseConnection,
    names: &[String],
    missing: MissingConstant,
) -> AppResult<serde_json::Map<String, serde_json::Value>> {
    let mut out = serde_json::Map::new();
    if names.is_empty() {
        return Ok(out);
    }
    let rows = constants::Entity::find()
        .filter(constants::Column::Name.is_in(names.to_vec()))
        .all(db)
        .await?;
    for constant in rows {
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

/// Pop the reserved context fields off a request body. They are calculation context, not tool
/// inputs: every tool accepts them and none receives them.
pub(super) fn take_context(
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
    let row = sites::Entity::find_by_id(site_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::BadRequest(format!("Site {site_id} not found")))?;
    let site_name = row.name.clone();
    let site = serde_json::to_value(row).map_err(|e| AppError::Internal(e.to_string()))?;

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

/// A built query as the statement the connection takes.
#[must_use]
pub fn build(query: &sea_orm::sea_query::SelectStatement) -> Statement {
    let (sql, values) = query.build(PostgresQueryBuilder);
    Statement::from_sql_and_values(sea_orm::DatabaseBackend::Postgres, sql, values)
}

/// The served spot value at one (site, parameter, instant): the sample mean, else the lowest
/// unflagged replicate that is not withdrawn. Each of the three is an expression, so a statement
/// resolving the parameter itself passes its own column where another passes a bound value.
#[must_use]
pub fn served_spot_value_expr(site: Expr, parameter: Expr, instant: Expr) -> Expr {
    use sea_orm::sea_query::ExprTrait;
    let smp = Alias::new("smp");
    let mean = Query::select()
        .column((smp.clone(), samples::Column::Mean))
        .from_as(samples::Entity, smp.clone())
        .and_where(Expr::col((smp.clone(), samples::Column::SiteId)).eq(site.clone()))
        .and_where(Expr::col((smp.clone(), samples::Column::ParameterId)).eq(parameter.clone()))
        .and_where(Expr::col((smp, samples::Column::CollectedAt)).eq(instant.clone()))
        .to_owned();

    let r = Alias::new("r");
    let lowest_replicate = Query::select()
        .expr(Func::coalesce([
            Expr::col((r.clone(), readings::Column::CalibratedValue)),
            Expr::col((r.clone(), readings::Column::RawValue)),
        ]))
        .from_as(readings::Entity, r.clone())
        .and_where(Expr::col((r.clone(), readings::Column::SiteId)).eq(site))
        .and_where(Expr::col((r.clone(), readings::Column::ParameterId)).eq(parameter))
        .and_where(Expr::col((r.clone(), readings::Column::Time)).eq(instant))
        .and_where(
            Expr::col((r.clone(), readings::Column::MeasurementType))
                .eq(crate::routes::private::readings::service::SPOT),
        )
        .and_where(Expr::cust(r#""r"."is_flagged" IS NOT TRUE"#))
        .and_where(Expr::col((r.clone(), readings::Column::WithdrawnAt)).is_null())
        .order_by((r, readings::Column::ReplicateIndex), Order::Asc)
        .limit(1)
        .to_owned();

    Func::coalesce([Expr::expr(mean), Expr::expr(lowest_replicate)]).into()
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
        let Some(parameter_id) = catalog_parameter_id(db, &e.parameter_code).await? else {
            continue;
        };
        let query = Query::select()
            .expr_as(
                served_spot_value_expr(
                    Expr::val(site_id),
                    Expr::val(parameter_id),
                    Expr::val(sea_orm::prelude::DateTimeWithTimeZone::from(collected_at)),
                ),
                Alias::new("value"),
            )
            .to_owned();
        let Some(row) = db.query_one_raw(build(&query)).await? else {
            continue;
        };
        let served = ServedSpotValue::from_query_result(&row, "")?;
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
    canonical_hash(&serde_json::json!({
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
    let mut trace: Vec<TraceStep> = Vec::new();
    let raw = if tool.engine == Engine::Formula {
        let numbers: std::collections::HashMap<String, f64> = effective_inputs
            .iter()
            .filter_map(|(k, v)| v.as_f64().map(|n| (k.clone(), n)))
            .collect();
        let constant_values: std::collections::HashMap<String, f64> = constants
            .iter()
            .filter_map(|(k, v)| v.as_f64().map(|n| (k.clone(), n)))
            .collect();
        let curve_values: std::collections::HashMap<String, Curve> = curves
            .iter()
            .filter_map(|(name, curve)| {
                let slope = curve.get("slope")?.as_f64()?;
                let intercept = curve.get("intercept")?.as_f64()?;
                Some((name.clone(), Curve { slope, intercept }))
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
        let (produced, steps) = evaluate_with_trace(
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
        trace = steps;
        let mut results = serde_json::Map::new();
        for entry in produced {
            match entry {
                Produced::Scalar(evaluated) => {
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
                Produced::PerReplicate { code, values, .. } => {
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
        trace,
    })
}

/// Split the script's result map into the values it produced and the outputs it cleared.
///
/// An NA arrives as null. The portal wrote NULL into that column rather than leaving the old
/// number standing, so a null is a clear, not an omission: the key leaves `results`, where every
/// consumer reads a value, and travels as `cleared`, which the chain turns into a withdrawal of
/// the stored output. An output the script never named is not in the map at all and is untouched.
pub(super) fn partition_cleared(
    results: &mut serde_json::Map<String, serde_json::Value>,
) -> Vec<String> {
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
pub(super) async fn apply_manifest_aggregates(
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
                        .unwrap_or(crate::routes::private::readings::service::SAMPLE),
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
/// Same ladder as the write path (`crate::routes::private::readings::service::resolve`), preceded by the one declaration that
/// belongs to the instant rather than the slot: an audit resolution scoped to a collection group
/// records its choice on that `samples` row, and the trigger computes the served sd from it. A
/// display resolved from the slot alone would show a different standard deviation for the same
/// values.
pub(super) async fn displayed_sd_estimator(
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
        && let Some(estimator) = crate::routes::private::readings::service::instant_declaration(
            db,
            site,
            parameter_id,
            at,
        )
        .await?
    {
        return Ok(Some(estimator));
    }
    let resolved =
        crate::routes::private::readings::service::resolve(db, site, parameter_id, None, None)
            .await?;
    Ok(resolved.is_declared().then_some(resolved.estimator))
}

/// The catalog parameter a manifest param names, matched the way every other code lookup does.
pub(super) async fn catalog_parameter_id(
    db: &DatabaseConnection,
    code: &str,
) -> AppResult<Option<Uuid>> {
    Ok(parameters::Entity::find()
        .select_only()
        .column(parameters::Column::Id)
        .filter(lowered_code_in(&[code.to_lowercase()]))
        .into_tuple()
        .one(db)
        .await?)
}

/// The serialization the API requires of the runner: full precision (the default rounds to 4
/// significant digits), scalars as scalars, and R NA as null.
pub(super) const RUNNER_JSON_ARGS: &str = "auto_unbox=true&digits=17&na=null";

#[derive(Deserialize)]
pub(super) struct RuntimeInfoResponse {
    #[serde(default)]
    r_version: Option<String>,
    #[serde(default)]
    image_build: Option<String>,
}

pub(super) fn runtime_cell() -> &'static tokio::sync::RwLock<Option<RunnerRuntime>> {
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

pub(super) async fn fetch_runtime_info(state: &AppState) -> Option<RunnerRuntime> {
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

pub(super) fn runner_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// R's empty list serialises as `[]`, which here means "no error" rather than a malformed one.
pub(super) fn deserialize_parse_error<'de, D: serde::Deserializer<'de>>(
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
pub(super) struct RunnerToolError {
    error: String,
    message: String,
    #[serde(default)]
    call: Option<String>,
    #[serde(default)]
    traceback: Vec<String>,
}

pub(super) fn parse_tool_error(body: &str) -> Option<RunnerToolError> {
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
pub(super) async fn call_runner(
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

/// The variables a formula reads that are not sources: the coefficients of its curve slot.
pub const CURVE_VARIABLES: [&str; 2] = ["curve_slope", "curve_intercept"];

/// Identifiers a formula may name that are neither a source nor a constant: meval's own
/// functions and constants, and the guard functions [`evaluate_formula`] registers.
pub const FORMULA_BUILTINS: &[&str] = &[
    "sqrt",
    "abs",
    "ln",
    "log",
    "exp",
    "sin",
    "cos",
    "tan",
    "asin",
    "acos",
    "atan",
    "sinh",
    "cosh",
    "tanh",
    "floor",
    "ceil",
    "round",
    "signum",
    "min",
    "max",
    "pi",
    "e",
    "if",
    "and",
    "or",
    "not",
    "lt",
    "le",
    "gt",
    "ge",
    "eq",
    "ne",
    "coalesce",
    "is_missing",
    "na",
];

/// The guard functions a missing value may pass through. Each is total over NaN: a variable read
/// only inside their arguments selects another arm or yields NA, and never leaves the expression
/// unevaluable.
const NAN_TOLERANT_GUARDS: &[&str] = &[
    "if",
    "and",
    "or",
    "not",
    "lt",
    "le",
    "gt",
    "ge",
    "eq",
    "ne",
    "coalesce",
    "is_missing",
];

/// Whether every read of `variable` in `formula` sits inside the arguments of a guard function.
/// A visit holding no value for such a variable binds it as NaN, the way `coalesce` takes its
/// second argument and a comparison against NA is false; any read outside a guard skips the
/// formula instead.
pub fn read_only_through_guards(formula: &str, variable: &str) -> bool {
    let chars: Vec<char> = formula.chars().collect();
    let mut guarded: Vec<bool> = Vec::new();
    let mut pending: Option<String> = None;
    let mut read = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_alphanumeric() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            pending = Some(chars[start..i].iter().collect());
            continue;
        }
        if c == '(' {
            // The identifier before a parenthesis names the call, not a value read.
            let call = pending.take();
            let inside = guarded.last().copied().unwrap_or(false)
                || call.is_some_and(|name| NAN_TOLERANT_GUARDS.contains(&name.as_str()));
            guarded.push(inside);
        } else {
            if pending.take().is_some_and(|name| name == variable) {
                if !guarded.last().copied().unwrap_or(false) {
                    return false;
                }
                read = true;
            }
            if c == ')' {
                guarded.pop();
            }
        }
        i += 1;
    }
    if pending.is_some_and(|name| name == variable) {
        return false;
    }
    read
}

/// Every identifier a formula names that the language does not define itself, in the order they
/// appear. Sources, constants and curve coefficients are all in here; which is which is decided
/// by the caller against what it holds.
pub fn free_identifiers(formula: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (i, c) in formula.char_indices() {
        if c.is_alphanumeric() || c == '_' {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            tokens.push(&formula[s..i]);
        }
    }
    if let Some(s) = start {
        tokens.push(&formula[s..]);
    }
    let mut seen: Vec<String> = Vec::new();
    for token in tokens {
        if token.chars().next().is_some_and(|c| c.is_ascii_digit())
            || FORMULA_BUILTINS.contains(&token)
            || seen.iter().any(|s| s == token)
        {
            continue;
        }
        seen.push(token.to_string());
    }
    seen
}

/// The constants a formula set reads: every free identifier that is neither one of the formula's
/// own source variables, a site property it reads, a step of the set, nor a curve coefficient. Sorted, so a manifest is the same document
/// whichever order the definitions were written in.
pub fn constants_of(formulas: &[PinnedFormula]) -> Vec<String> {
    // A step reaches the formulas after it under its own code, so that code is never a constant.
    let steps: Vec<&str> = formulas
        .iter()
        .filter(|f| f.intermediate)
        .map(|f| f.code.as_str())
        .collect();
    let mut names: Vec<String> = Vec::new();
    for formula in formulas {
        for identifier in free_identifiers(&formula.formula) {
            if formula.sources.iter().any(|(v, _)| *v == identifier)
                || formula.site_sources.iter().any(|(v, _)| *v == identifier)
                || steps.contains(&identifier.as_str())
                || CURVE_VARIABLES.contains(&identifier.as_str())
                || names.contains(&identifier)
            {
                continue;
            }
            names.push(identifier);
        }
    }
    names.sort();
    names
}

/// The curve slots a formula set declares, in evaluation order and deduplicated.
pub fn curve_slots(formulas: &[PinnedFormula]) -> Vec<String> {
    let mut slots: Vec<String> = Vec::new();
    for formula in in_order(formulas).unwrap_or_else(|_| formulas.iter().collect()) {
        if let Some(slot) = &formula.curve_slot
            && !slots.contains(slot)
        {
            slots.push(slot.clone());
        }
    }
    slots
}

/// Formulas in the order they evaluate: producers before consumers, the same relation
/// `dependency_order` and `build_evaluation_order` sort tools and derived parameters by. An
/// edge A→B exists where A's output parameter is one of B's sources, and where A is a step whose
/// code B names as a free identifier: a step reaches its readers under its own code and produces
/// no parameter, so the identifier is the only thing that can order it. `ordinal` then `code` is
/// the tie-break among formulas nothing orders, never the order itself: it is hand-set, defaults
/// to 0 for every formula the authoring form creates, and ordering by it alone made a dependent
/// formula read the store instead of the value just produced.
///
/// A cycle has no runnable order and is returned naming its members, the way the other two engines
/// answer one.
pub fn in_order(formulas: &[PinnedFormula]) -> Result<Vec<&PinnedFormula>, String> {
    let mut candidates: Vec<&PinnedFormula> = formulas.iter().collect();
    candidates.sort_by(|a, b| a.ordinal.cmp(&b.ordinal).then_with(|| a.code.cmp(&b.code)));

    let produces: Vec<Option<String>> = candidates
        .iter()
        .map(|f| f.output_parameter_code.as_ref().map(|c| c.to_lowercase()))
        .collect();
    let consumes: Vec<Vec<String>> = candidates
        .iter()
        .map(|f| f.sources.iter().map(|(_, p)| p.to_lowercase()).collect())
        .collect();
    // What each formula names in its own text, which is how it reaches a step.
    let names: Vec<Vec<String>> = candidates
        .iter()
        .map(|f| {
            free_identifiers(&f.formula)
                .into_iter()
                .map(|i| i.to_lowercase())
                .collect()
        })
        .collect();
    let steps: Vec<Option<String>> = candidates
        .iter()
        .map(|f| f.intermediate.then(|| f.code.to_lowercase()))
        .collect();

    let n = candidates.len();
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n];
    for b in 0..n {
        for (a, produced) in produces.iter().enumerate() {
            if a != b
                && let Some(code) = produced
                && consumes[b].contains(code)
            {
                deps[b].push(a);
            }
        }
        for (a, step) in steps.iter().enumerate() {
            if a != b
                && let Some(code) = step
                && names[b].contains(code)
                && !deps[b].contains(&a)
            {
                deps[b].push(a);
            }
        }
    }

    let mut ordered = Vec::with_capacity(n);
    let mut placed = vec![false; n];
    loop {
        let mut progressed = false;
        for i in 0..n {
            if !placed[i] && deps[i].iter().all(|&d| placed[d]) {
                placed[i] = true;
                ordered.push(candidates[i]);
                progressed = true;
            }
        }
        if ordered.len() == n {
            return Ok(ordered);
        }
        if !progressed {
            let cycle: Vec<&str> = (0..n)
                .filter(|&i| !placed[i])
                .map(|i| candidates[i].code.as_str())
                .collect();
            return Err(format!(
                "formulas form a dependency cycle: {}",
                cycle.join(", ")
            ));
        }
    }
}

/// Parameter codes a calculation produces before the formula at `index` runs. Those are satisfied
/// from inside the calculation, so they are not declared as event inputs and are not resolved from
/// stored readings.
///
/// A per-replicate output is one of them only for another per-replicate formula, which reads it at
/// its own index. A scalar consumer reads the family's mean, which the `samples` trigger derives
/// after the repeats are stored and never a formula (Q95, D21), so for that one the output stays an
/// event input and the second stage converges on the pass after the repeats land.
pub(super) fn produced_before(
    ordered: &[&PinnedFormula],
    index: usize,
    consumer_is_per_replicate: bool,
) -> Vec<String> {
    ordered[..index]
        .iter()
        .filter(|f| f.per_replicate.is_none() || consumer_is_per_replicate)
        .filter_map(|f| f.output_parameter_code.as_ref())
        .map(|c| c.to_lowercase())
        .collect()
}

/// The replicated codes of the calculation's parameter group, empty where it declares none.
pub async fn replicated_for<C: ConnectionTrait>(
    db: &C,
    parameter_group_id: Option<Uuid>,
) -> AppResult<Vec<String>> {
    let Some(group_id) = parameter_group_id else {
        return Ok(Vec::new());
    };
    Ok(crate::routes::private::parameter_groups::service::replicated_codes(db, group_id).await?)
}

/// The manifest a formula calculation presents, in the same JSON shape an authored manifest is
/// written in, so it parses and validates through the one manifest parser.
///
/// Each distinct source becomes a number param and an event input, except a source reading a
/// parameter an earlier formula produces: that value comes from the evaluation, not from the
/// store, so declaring it would make a first run refuse for want of a reading nothing has written
/// yet.
///
/// `replicated` names the catalog codes the calculation's parameter group holds several values of
/// per visit. A source of one of those, read by a formula that walks the replicates, is the family
/// rather than a number: the portal's own sets read a second family at the same letter, and the
/// engine binds every family at each index. A family only ever read by a scalar formula stays a
/// number and resolves to the group's served value, which is its mean (Q155).
pub fn manifest_json(
    label: &str,
    description: Option<&str>,
    formulas: &[PinnedFormula],
    replicated: &[String],
) -> Result<serde_json::Value, String> {
    let ordered = in_order(formulas)?;
    let driven: Vec<String> = ordered
        .iter()
        .filter_map(|f| f.per_replicate.clone())
        .collect();
    let is_replicated = |code: &str| replicated.iter().any(|c| c.eq_ignore_ascii_case(code));
    let walked: Vec<&String> = ordered
        .iter()
        .filter(|f| f.per_replicate.is_some())
        .flat_map(|f| f.sources.iter().map(|(variable, _)| variable))
        .collect();
    let mut params = Vec::new();
    let mut event_inputs = Vec::new();
    let mut site_inputs = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (index, formula) in ordered.iter().enumerate() {
        let internal = produced_before(&ordered, index, formula.per_replicate.is_some());
        for (variable, parameter_code) in &formula.sources {
            if internal.contains(&parameter_code.to_lowercase()) || seen.contains(variable) {
                continue;
            }
            seen.push(variable.clone());
            if driven.contains(variable)
                || (walked.contains(&variable) && is_replicated(parameter_code))
            {
                // A variable a formula evaluates over is the family, not one number: the body
                // carries the whole list, and the param names the parameter those readings are
                // of. It is deliberately not an event input as well, because resolving one would
                // put the group's single served value into a field that holds the repeats.
                params.push(json!({
                    "name": variable,
                    "label": variable,
                    "kind": "replicates",
                    "parameter_code": parameter_code,
                    "required": false,
                }));
                continue;
            }
            params.push(json!({
                "name": variable,
                "label": variable,
                "kind": "number",
                "required": false,
            }));
            event_inputs.push(json!({ "param": variable, "parameter_code": parameter_code }));
        }
        for (variable, property) in &formula.site_sources {
            if seen.contains(variable) {
                continue;
            }
            seen.push(variable.clone());
            params.push(json!({
                "name": variable,
                "label": variable,
                "kind": "number",
                "required": false,
            }));
            // Required: the formula reads it in every run, so a site that holds no value refuses
            // the run naming the property rather than evaluating to nothing. The param itself
            // stays optional, which is what lets a request override the stored value.
            site_inputs.push(json!({
                "property": property,
                "param": variable,
                "required": true,
            }));
        }
    }
    // An intermediate is no output: it saves nowhere, so a manifest naming it would offer to store
    // a step of the arithmetic.
    let outputs: Vec<serde_json::Value> = ordered
        .iter()
        .filter(|f| !f.intermediate)
        .map(|f| {
            let mut output = json!({
                "key": f.code,
                "label": f.label,
                "units": f.units,
                "suggested_parameter_code": f.output_parameter_code,
            });
            // Declared only when true: an output that says nothing is one number, which is what
            // every manifest written before this meant.
            if f.per_replicate.is_some() {
                output["per_replicate"] = json!(true);
            }
            output
        })
        .collect();
    let curves: Vec<serde_json::Value> = curve_slots(formulas)
        .into_iter()
        .map(|slot| json!({ "name": slot, "label": slot, "required": false }))
        .collect();

    Ok(json!({
        "label": label,
        "description": description,
        "params": params,
        "outputs": outputs,
        "constants": constants_of(formulas),
        "curves": curves,
        "site_inputs": site_inputs,
        "event_inputs": event_inputs,
    }))
}

/// The version body of a formula calculation: its formula set in evaluation order, as JSON.
///
/// A `tool_script_versions` row holds this where a script calculation holds R, so a version is
/// self-contained: the run that pins it can be replayed from the version alone, without reading
/// the definitions as they stand today.
pub fn render(formulas: &[PinnedFormula]) -> Result<String, String> {
    let ordered = in_order(formulas)?;
    Ok(serde_json::to_string_pretty(&ordered).unwrap_or_else(|_| "[]".to_string()))
}

/// The formula set a stored version body holds.
pub fn parse_pinned(body: &str) -> Result<Vec<PinnedFormula>, String> {
    serde_json::from_str(body).map_err(|e| format!("unreadable formula set: {e}"))
}

/// Evaluate every formula in order, feeding each result forward under the parameter code it is
/// stored as, so a formula reading an earlier formula's output takes the fresh value.
///
/// `inputs` is keyed by variable name, the form the resolved run holds; `constants` and `curves`
/// are what the manifest declared, resolved by the caller. The result is one entry per formula,
/// in evaluation order, keyed by output code, the form a run outcome holds.
///
/// A formula whose sources do not all resolve is skipped and the rest still evaluate: the portal
/// warns and moves to the next calculation rather than losing the row. A formula reading a
/// skipped formula's output skips in turn. Only an unevaluable expression is fatal, because that
/// is the definition being wrong rather than the visit being incomplete.
///
/// A source the formula reads only through a guard function is bound as NaN rather than skipped,
/// so the portal's defaults and its comparisons against a missing value take the arm they take
/// there ([`read_only_through_guards`]).
pub fn evaluate(
    formulas: &[PinnedFormula],
    inputs: &HashMap<String, f64>,
    constants: &HashMap<String, f64>,
    curves: &HashMap<String, Curve>,
) -> Result<Vec<Evaluated>, String> {
    evaluate_set(formulas, inputs, constants, curves, false)
}

/// One pass over the formula set. `chain_replicates` is the per-index pass of
/// [`evaluate_over_replicates`]: a per-replicate result is handed to a later per-replicate formula
/// at the same index, and to nothing else.
pub(super) fn evaluate_set(
    formulas: &[PinnedFormula],
    inputs: &HashMap<String, f64>,
    constants: &HashMap<String, f64>,
    curves: &HashMap<String, Curve>,
    chain_replicates: bool,
) -> Result<Vec<Evaluated>, String> {
    let ordered = in_order(formulas)?;
    let mut produced: HashMap<String, f64> = HashMap::new();
    let mut at_index: HashMap<String, f64> = HashMap::new();
    // An intermediate stores nothing, so it is named by its own code rather than by a parameter,
    // and reaches a later formula as a variable of that name.
    let mut steps: HashMap<String, f64> = HashMap::new();
    let mut steps_at_index: HashMap<String, f64> = HashMap::new();
    let mut results = Vec::with_capacity(ordered.len());
    for formula in &ordered {
        let mut variables: HashMap<String, f64> = constants.clone();
        variables.extend(steps.iter().map(|(k, v)| (k.clone(), *v)));
        if formula.per_replicate.is_some() && chain_replicates {
            variables.extend(steps_at_index.iter().map(|(k, v)| (k.clone(), *v)));
        }
        let mut skipped = None;
        for (variable, parameter_code) in &formula.sources {
            let code = parameter_code.to_lowercase();
            let chained = formula
                .per_replicate
                .is_some()
                .then(|| at_index.get(&code))
                .flatten();
            let value = chained
                .or_else(|| produced.get(&code))
                .or_else(|| inputs.get(variable))
                .copied();
            match value {
                Some(value) => {
                    variables.insert(variable.clone(), value);
                }
                None if read_only_through_guards(&formula.formula, variable) => {
                    variables.insert(variable.clone(), f64::NAN);
                }
                None => {
                    skipped = Some(format!("no value for {variable} ({parameter_code})"));
                    break;
                }
            }
        }
        // A site property arrives resolved as an input under the variable's name.
        if skipped.is_none() {
            for (variable, property) in &formula.site_sources {
                match inputs.get(variable) {
                    Some(value) => {
                        variables.insert(variable.clone(), *value);
                    }
                    None if read_only_through_guards(&formula.formula, variable) => {
                        variables.insert(variable.clone(), f64::NAN);
                    }
                    None => {
                        skipped = Some(format!("no value for {variable} (site {property})"));
                        break;
                    }
                }
            }
        }
        if let Some(slot) = &formula.curve_slot
            && skipped.is_none()
        {
            match curves.get(slot) {
                Some(curve) => {
                    variables.insert(CURVE_VARIABLES[0].to_string(), curve.slope);
                    variables.insert(CURVE_VARIABLES[1].to_string(), curve.intercept);
                }
                None => skipped = Some(format!("curve '{slot}' was not supplied")),
            }
        }
        if let Some(reason) = skipped {
            results.push(Evaluated {
                code: formula.code.clone(),
                value: None,
                curve_slot: formula.curve_slot.clone(),
                skipped: Some(reason),
                bindings: Vec::new(),
            });
            continue;
        }
        let value = evaluate_formula(&formula.formula, &variables)
            .map_err(|e| format!("formula {}: {e}", formula.code))?;
        let bindings = free_identifiers(&formula.formula)
            .into_iter()
            .filter_map(|name| variables.get(&name).map(|v| (name, *v)))
            .collect();
        // NaN is the portal's NA: computed, and not a number. It clears the stored value rather
        // than feeding the next formula, which would turn one NA into a whole calculation of them.
        // A per-replicate value is one repeat, so it travels only to a later per-replicate formula
        // at this index; a scalar formula reading that parameter takes the stored mean instead.
        if !value.is_nan() {
            if formula.intermediate {
                if formula.per_replicate.is_none() {
                    steps.insert(formula.code.clone(), value);
                } else if chain_replicates {
                    steps_at_index.insert(formula.code.clone(), value);
                }
            } else if let Some(code) = &formula.output_parameter_code {
                if formula.per_replicate.is_none() {
                    produced.insert(code.to_lowercase(), value);
                } else if chain_replicates {
                    at_index.insert(code.to_lowercase(), value);
                }
            }
        }
        results.push(Evaluated {
            code: formula.code.clone(),
            value: (!value.is_nan()).then_some(value),
            curve_slot: formula.curve_slot.clone(),
            skipped: None,
            bindings,
        });
    }
    Ok(results)
}

/// The number of replicate indexes a calculation runs over: the longest replicate vector any
/// per-replicate formula names. Nothing declared per-replicate means a width of one, which is the
/// scalar case running once.
pub(super) fn replicate_width(
    formulas: &[&PinnedFormula],
    replicates: &HashMap<String, Vec<Option<f64>>>,
) -> usize {
    formulas
        .iter()
        .filter_map(|f| family_width(f, formulas, replicates))
        .max()
        .unwrap_or(1)
        .max(1)
}

/// How many indexes one per-replicate formula runs over: the length of the entered family it names,
/// or, where it names an earlier formula's per-replicate output, that producer's width.
pub(super) fn family_width(
    formula: &PinnedFormula,
    formulas: &[&PinnedFormula],
    replicates: &HashMap<String, Vec<Option<f64>>>,
) -> Option<usize> {
    let variable = formula.per_replicate.as_ref()?;
    if let Some(values) = replicates.get(variable) {
        return Some(values.len());
    }
    let code = formula
        .sources
        .iter()
        .find(|(name, _)| name == variable)
        .map(|(_, code)| code.to_lowercase())?;
    // The set is topologically ordered by `in_order`, so a producer is always earlier and the walk
    // terminates.
    let producer = formulas.iter().find(|f| {
        f.output_parameter_code
            .as_ref()
            .is_some_and(|produced| produced.to_lowercase() == code)
    })?;
    family_width(producer, formulas, replicates)
}

/// Evaluate a calculation whose formulas may be per-replicate, running the whole set once per
/// index and assembling one entry per output.
///
/// A scalar formula is evaluated at index 0 and reported once: it reads the replicate group's
/// statistics or a scalar input, not one repeat. A per-replicate formula is reported as a vector
/// the width of the family, holding `None` where that index had no value, so a gap at index 1
/// stays at index 1.
///
/// `inputs` are the scalars, `replicates` the vectors keyed by the same variable names. A variable
/// present in both takes its indexed value, because a formula that declared itself per-replicate
/// asked for the repeat rather than the summary.
pub fn evaluate_over_replicates(
    formulas: &[PinnedFormula],
    inputs: &HashMap<String, f64>,
    replicates: &HashMap<String, Vec<Option<f64>>>,
    constants: &HashMap<String, f64>,
    curves: &HashMap<String, Curve>,
) -> Result<Vec<Produced>, String> {
    evaluate_with_trace(formulas, inputs, replicates, constants, curves).map(|(p, _)| p)
}

/// [`evaluate_over_replicates`], also returning each formula as it was evaluated: its text and,
/// per cell, the value and the variables it read. A scalar formula is one cell with no index; a
/// per-replicate one is a cell per index.
pub fn evaluate_with_trace(
    formulas: &[PinnedFormula],
    inputs: &HashMap<String, f64>,
    replicates: &HashMap<String, Vec<Option<f64>>>,
    constants: &HashMap<String, f64>,
    curves: &HashMap<String, Curve>,
) -> Result<(Vec<Produced>, Vec<TraceStep>), String> {
    let ordered = in_order(formulas)?;
    let width = replicate_width(&ordered, replicates);
    let mut per_index: Vec<Vec<Evaluated>> = Vec::with_capacity(width);
    for index in 0..width {
        let mut at_index = inputs.clone();
        for (variable, values) in replicates {
            match values.get(index).copied().flatten() {
                Some(value) => {
                    at_index.insert(variable.clone(), value);
                }
                // A repeat that was not measured is absent, which skips the formulas reading it
                // and leaves this index a gap rather than falling back to the group's summary.
                None => {
                    at_index.remove(variable);
                }
            }
        }
        per_index.push(evaluate_set(formulas, &at_index, constants, curves, true)?);
    }

    let cell = |index: Option<usize>, evaluated: &Evaluated| TraceCell {
        index,
        value: evaluated.value,
        skipped: evaluated.skipped.clone(),
        bindings: evaluated.bindings.iter().cloned().collect(),
    };
    let mut produced = Vec::with_capacity(ordered.len());
    let mut trace = Vec::with_capacity(ordered.len());
    for (position, formula) in ordered.iter().enumerate() {
        let cells = if formula.per_replicate.is_none() {
            produced.push(Produced::Scalar(per_index[0][position].clone()));
            vec![cell(None, &per_index[0][position])]
        } else {
            produced.push(Produced::PerReplicate {
                code: formula.code.clone(),
                values: per_index
                    .iter()
                    .map(|results| results[position].value)
                    .collect(),
                curve_slot: formula.curve_slot.clone(),
            });
            per_index
                .iter()
                .enumerate()
                .map(|(i, results)| cell(Some(i), &results[position]))
                .collect()
        };
        trace.push(TraceStep {
            code: formula.code.clone(),
            label: formula.label.clone(),
            units: formula.units.clone(),
            formula: formula.formula.clone(),
            intermediate: formula.intermediate,
            per_replicate: formula.per_replicate.is_some(),
            cells,
        });
    }
    Ok((produced, trace))
}

/// The global parameters a subject moves.
///
/// Metadata only. A calibration's parameters come from its own declaration and its instrument's
/// deployments rather than from a scan of the readings it corrected: the question is asked before
/// an edit, on a page that must answer in one round trip, and the two agree wherever the
/// attribution is right.
pub async fn parameters_of(db: &DatabaseConnection, subject: &Subject) -> AppResult<Vec<Uuid>> {
    let (sql, values): (&str, Vec<sea_orm::Value>) = match subject {
        Subject::Parameters(ids) => return Ok(ids.clone()),
        Subject::Calibration(id) => (
            "SELECT DISTINCT p FROM (
               SELECT c.parameter_id AS p FROM sensor_calibrations c WHERE c.id = $1
               UNION ALL
               SELECT d.parameter_id FROM sensor_deployments d
                 JOIN sensor_calibrations c ON c.sensor_id = d.sensor_id
                WHERE c.id = $1 AND c.parameter_id IS NULL
             ) q WHERE p IS NOT NULL",
            vec![(*id).into()],
        ),
        Subject::Slot(id) => (
            "SELECT parameter_id AS p FROM site_parameters WHERE id = $1",
            vec![(*id).into()],
        ),
        // A replicate of a group is the same parameter as its siblings, so the index the caller
        // holds the reading by does not narrow the answer.
        Subject::Reading { stream_id, .. } => (
            "SELECT sp.parameter_id AS p
               FROM data_streams s
               JOIN site_parameters sp ON sp.id = s.site_parameter_id
              WHERE s.id = $1",
            vec![(*stream_id).into()],
        ),
        Subject::Calculation(name) => (
            "SELECT DISTINCT out.id AS p
               FROM tool_scripts s
               JOIN tool_script_versions v ON v.id = s.active_version_id
               CROSS JOIN LATERAL jsonb_array_elements(COALESCE(v.manifest->'outputs', '[]'::jsonb)) o
               JOIN parameters out
                 ON LOWER(out.code) = LOWER(o->>'suggested_parameter_code')
              WHERE s.name = $1",
            vec![name.clone().into()],
        ),
    };
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    let mut ids = Vec::with_capacity(rows.len());
    for row in &rows {
        ids.push(row.try_get::<Uuid>("", "p")?);
    }
    Ok(ids)
}

/// [`calculations_fed_by`] for any subject.
pub async fn calculations_fed_by_subject(
    db: &DatabaseConnection,
    subject: &Subject,
) -> AppResult<Vec<CalculationImpact>> {
    let ids = parameters_of(db, subject).await?;
    calculations_fed_by(db, &ids).await
}

/// Every enabled calculation that reads one of `parameter_ids` at a visit, directly or through a
/// calculation downstream of it, in the order the chain would run them. Empty when no calculation
/// reads any of them, which is the common case for a logger parameter.
pub async fn calculations_fed_by(
    db: &DatabaseConnection,
    parameter_ids: &[Uuid],
) -> AppResult<Vec<CalculationImpact>> {
    if parameter_ids.is_empty() {
        return Ok(Vec::new());
    }
    let tools = list_active_tools(db).await?;
    if tools.iter().all(|t| t.manifest.event_inputs.is_empty()) {
        return Ok(Vec::new());
    }
    let catalog = load_parameter_catalog(db, tools.iter().map(|t| &t.manifest)).await?;
    let order = dependency_order(&tools, &catalog)?;

    let touched: Vec<ImpactParameter> = parameters::Entity::find()
        .filter(parameters::Column::Id.is_in(parameter_ids.to_vec()))
        .all(db)
        .await?
        .into_iter()
        .map(|p| ImpactParameter {
            parameter_id: p.id,
            parameter_code: p.code,
        })
        .collect();
    let mut impacts = fed_closure(&tools, &catalog, &order, &touched);
    impacts.extend(derived_fed_by(db, &touched).await?);
    Ok(impacts)
}

/// One standalone derived definition as an edge of the same graph: what it reads and what it
/// writes.
///
/// A derived parameter attached to a calculation is already in the manifest graph, because a
/// formula calculation presents one. A standalone definition (`tool_script_id IS NULL` and not a
/// step) is the continuous kind the derived job and the janitor serve, and it has no manifest, so
/// its dependants were invisible to the closure entirely. A shared step belongs to no calculation
/// either (Q156) and is not one of these: it mints no parameter, so it writes no edge.
pub(super) struct DerivedEdge {
    code: String,
    label: String,
    reads: Vec<String>,
    output: Option<ImpactParameter>,
}

/// One standalone definition as the query selects it: the parameters it reads arrive as a JSON
/// array of codes, and the output is a `LEFT JOIN` so a definition with no output parameter is
/// legitimately two nulls.
#[derive(FromQueryResult)]
pub(super) struct DerivedEdgeRow {
    code: String,
    name: String,
    output_id: Option<Uuid>,
    output_code: Option<String>,
    reads: serde_json::Value,
}

pub(super) async fn derived_edges(db: &DatabaseConnection) -> AppResult<Vec<DerivedEdge>> {
    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT d.code, d.name, out.id AS output_id, out.code AS output_code,
                    COALESCE(
                      (SELECT jsonb_agg(p.code)
                         FROM derived_parameter_sources src
                         JOIN parameters p ON p.id = src.parameter_id
                        WHERE src.derived_definition_id = d.id),
                      '[]'::jsonb) AS reads
               FROM calculation_formulas d
               LEFT JOIN parameters out ON out.id = d.output_parameter_id
              WHERE d.tool_script_id IS NULL AND d.intermediate = false
              ORDER BY d.code"
                .to_string(),
        ))
        .await?;
    let mut edges = Vec::with_capacity(rows.len());
    for row in &rows {
        let DerivedEdgeRow {
            code,
            name,
            output_id,
            output_code,
            reads,
        } = DerivedEdgeRow::from_query_result(row, "")?;
        edges.push(DerivedEdge {
            code,
            label: name,
            reads: reads
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_lowercase))
                        .collect()
                })
                .unwrap_or_default(),
            output: match (output_id, output_code) {
                (Some(parameter_id), Some(parameter_code)) => Some(ImpactParameter {
                    parameter_id,
                    parameter_code,
                }),
                _ => None,
            },
        });
    }
    Ok(edges)
}

/// The standalone derived definitions a touched set feeds, in the same shape a calculation
/// answers in. The walk repeats until nothing new is reachable, so a derived parameter feeding
/// another is followed however the definitions happen to be ordered.
pub(super) async fn derived_fed_by(
    db: &DatabaseConnection,
    touched: &[ImpactParameter],
) -> AppResult<Vec<CalculationImpact>> {
    let edges = derived_edges(db).await?;
    if edges.is_empty() {
        return Ok(Vec::new());
    }
    Ok(derived_closure(&edges, touched))
}

pub(super) fn derived_closure(
    edges: &[DerivedEdge],
    touched: &[ImpactParameter],
) -> Vec<CalculationImpact> {
    let mut reachable: HashMap<String, Vec<ImpactParameter>> = HashMap::new();
    for p in touched {
        reachable.insert(p.parameter_code.to_lowercase(), vec![p.clone()]);
    }
    let mut impacts: Vec<CalculationImpact> = Vec::new();
    // A definition reading another's output is followed by re-walking until the reachable set
    // stops growing: the definitions carry no order of their own, unlike the manifest tools.
    loop {
        let before = impacts.len();
        for edge in edges {
            if impacts.iter().any(|i| i.tool == edge.code) {
                continue;
            }
            let mut reads: Vec<ImpactParameter> = Vec::new();
            for code in &edge.reads {
                for root in reachable.get(code).into_iter().flatten() {
                    if !reads.iter().any(|x| x.parameter_id == root.parameter_id) {
                        reads.push(root.clone());
                    }
                }
            }
            if reads.is_empty() {
                continue;
            }
            if let Some(output) = &edge.output {
                reachable
                    .entry(output.parameter_code.to_lowercase())
                    .or_default()
                    .extend(reads.iter().cloned());
            }
            impacts.push(CalculationImpact {
                tool: edge.code.clone(),
                label: edge.label.clone(),
                reads,
                outputs: edge.output.iter().cloned().collect(),
            });
        }
        if impacts.len() == before {
            return impacts;
        }
    }
}

/// The closure walk itself: with tools in run order, a tool is fed when an `event_input` names a
/// touched parameter or an output of a tool already fed; its outputs then count as reachable for
/// the tools after it.
pub fn fed_closure(
    tools: &[ActiveTool],
    catalog: &ParameterCatalog,
    order: &[usize],
    touched: &[ImpactParameter],
) -> Vec<CalculationImpact> {
    // Reachable parameter code → the touched parameters it descends from.
    let mut reachable: HashMap<String, Vec<ImpactParameter>> = HashMap::new();
    for p in touched {
        reachable.insert(p.parameter_code.to_lowercase(), vec![p.clone()]);
    }

    let mut impacts = Vec::new();
    for &i in order {
        let tool = &tools[i];
        let mut reads: Vec<ImpactParameter> = Vec::new();
        for e in &tool.manifest.event_inputs {
            if let Some(roots) = reachable.get(&e.parameter_code.to_lowercase()) {
                for r in roots {
                    if !reads.iter().any(|x| x.parameter_id == r.parameter_id) {
                        reads.push(r.clone());
                    }
                }
            }
        }
        if reads.is_empty() {
            continue;
        }
        let outputs: Vec<ImpactParameter> = tool
            .manifest
            .outputs
            .iter()
            .filter_map(|o| catalog.resolve(o))
            .map(|p| ImpactParameter {
                parameter_id: p.id,
                parameter_code: p.code.clone(),
            })
            .collect();
        for o in &outputs {
            reachable
                .entry(o.parameter_code.to_lowercase())
                .or_default()
                .extend(reads.iter().cloned());
        }
        impacts.push(CalculationImpact {
            tool: tool.name.clone(),
            label: tool.label.clone(),
            reads,
            outputs,
        });
    }
    impacts
}

/// Serialise a value with object keys in sorted order, so two equivalent manifests produce the
/// same bytes whatever order they were written in.
pub(super) fn canonical_json(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::Value::String((*key).clone()).to_string());
                out.push(':');
                canonical_json(&map[*key], out);
            }
            out.push('}');
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_json(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// The identity of a version's whole content, not just its R source: an edit to the manifest or
/// the test cases is a new version, and only a re-post of everything unchanged is a duplicate.
/// Prefixed because rows created before this hashed `md5(script)` alone and keep that value.
pub fn version_content_hash(
    script: &str,
    entry_function: &str,
    manifest: &serde_json::Value,
    test_cases: &serde_json::Value,
) -> String {
    let bundle = serde_json::json!({
        "script": script,
        "entry_function": entry_function,
        "manifest": manifest,
        "test_cases": test_cases,
    });
    let mut canonical = String::new();
    canonical_json(&bundle, &mut canonical);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

/// The sha256 of a value's canonical serialisation, for any record that has to be compared by
/// content rather than by the bytes it happened to arrive in.
pub fn canonical_hash(value: &serde_json::Value) -> String {
    let mut canonical = String::new();
    canonical_json(value, &mut canonical);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

/// Hash a version over the form the database will hold, so the hash on a row can be recomputed
/// from that row and match.
///
/// `jsonb` is a parsed value, not the text it arrived as: it re-renders numbers through `numeric`
/// (`1e-9` reads back as `0.000000001`), drops insignificant whitespace, orders keys its own way
/// and keeps only the last of a repeated key. Hashing the value as it sat in memory therefore
/// identifies the bytes an author sent rather than the bytes anyone can read back, which makes
/// both the provenance hash and the duplicate-version check unverifiable: fetching a version and
/// re-posting it produces a different hash and so a second copy. Rather than reimplement those
/// rules (they are Postgres', and they cover more than floats), this asks Postgres to apply them
/// and hashes the answer.
///
/// Costs one round trip before the insert. The alternative, inserting and then hashing what came
/// back, needs a transaction to keep a row that failed the duplicate check from existing.
/// The manifest and cases as the database normalises them, still text.
#[derive(FromQueryResult)]
pub(super) struct Normalised {
    manifest: String,
    test_cases: String,
}

pub async fn stored_version_content<C: ConnectionTrait>(
    db: &C,
    script: &str,
    entry_function: &str,
    manifest: &serde_json::Value,
    test_cases: &serde_json::Value,
) -> Result<StoredVersionContent, DbErr> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm_migration::sea_orm::DatabaseBackend::Postgres,
            "SELECT $1::jsonb::text AS manifest, $2::jsonb::text AS test_cases",
            [
                serde_json::to_string(manifest)
                    .map_err(|e| DbErr::Custom(format!("manifest is not serialisable: {e}")))?
                    .into(),
                serde_json::to_string(test_cases)
                    .map_err(|e| DbErr::Custom(format!("test cases are not serialisable: {e}")))?
                    .into(),
            ],
        ))
        .await?
        .ok_or_else(|| {
            DbErr::Custom("normalising the version content returned no row".to_string())
        })?;
    let Normalised {
        manifest,
        test_cases,
    } = Normalised::from_query_result(&row, "")?;
    let content_hash = version_content_hash(
        script,
        entry_function,
        &serde_json::from_str(&manifest)
            .map_err(|e| DbErr::Custom(format!("normalised manifest is not JSON: {e}")))?,
        &serde_json::from_str(&test_cases)
            .map_err(|e| DbErr::Custom(format!("normalised test cases are not JSON: {e}")))?,
    );
    Ok(StoredVersionContent {
        content_hash,
        manifest,
        test_cases,
    })
}

pub(super) fn parse_ids(csv: Option<&str>) -> AppResult<Vec<Uuid>> {
    csv.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Uuid::parse_str)
        .collect::<Result<_, _>>()
        .map_err(|_| AppError::BadRequest("parameter_ids must be UUIDs".to_string()))
}

/// Every parameter any active calculation reads or writes, which is what coverage is reported over.
///
/// The outputs resolve through the tool catalog, which is the same resolution a run uses. The
/// `event_inputs` are looked up by code directly: the catalog is loaded from output and param
/// codes only, so an input parameter named nowhere else is absent from it.
pub(super) async fn calculation_slots(state: &AppState) -> AppResult<Vec<Uuid>> {
    let tools = list_active_tools(&state.db).await?;
    let catalog = load_parameter_catalog(&state.db, tools.iter().map(|t| &t.manifest)).await?;
    let mut ids: Vec<Uuid> = Vec::new();
    let mut input_codes: Vec<String> = Vec::new();
    for tool in &tools {
        for e in &tool.manifest.event_inputs {
            input_codes.push(e.parameter_code.to_lowercase());
        }
        for o in &tool.manifest.outputs {
            if let Some(p) = catalog.resolve(o)
                && !ids.contains(&p.id)
            {
                ids.push(p.id);
            }
        }
    }
    if !input_codes.is_empty() {
        for id in parameter_ids_of_codes(&state.db, input_codes).await? {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    Ok(ids)
}

/// Coverage for a set of parameters: how many slots declare each, and what its readings and their
/// provenance say. Each side is its own LATERAL, so a parameter with no readings still reports a
/// row with zeroes rather than dropping out of the join.
pub(super) fn coverage_query(parameter_ids: &[Uuid], site_id: Option<Uuid>) -> Statement {
    use sea_orm::sea_query::ExprTrait;

    let p = Alias::new("p");
    let sp = Alias::new("sp");
    let r = Alias::new("r");
    let ds = Alias::new("ds");
    let cfg = Alias::new("cfg");
    let obs = Alias::new("obs");

    let mut configured = Query::select();
    configured
        .expr_as(
            Expr::cust(r#"COUNT(*)::bigint"#),
            Alias::new("sites_configured"),
        )
        .from_as(site_parameters::Entity, sp.clone())
        .and_where(
            Expr::col((sp.clone(), site_parameters::Column::ParameterId))
                .equals((p.clone(), parameters::Column::Id)),
        );
    if let Some(id) = site_id {
        configured.and_where(Expr::col((sp, site_parameters::Column::SiteId)).eq(id));
    }

    let mut observed = Query::select();
    observed
        .expr_as(Expr::cust(r#"COUNT(*)::bigint"#), Alias::new("reading_count"))
        .expr_as(
            Expr::col((r.clone(), readings::Column::Time)).min(),
            Alias::new("first_reading"),
        )
        .expr_as(
            Expr::col((r.clone(), readings::Column::Time)).max(),
            Alias::new("last_reading"),
        )
        .expr_as(
            Expr::cust(
                r#"ARRAY_AGG(DISTINCT "ds"."source_system") FILTER (WHERE "ds"."source_system" IS NOT NULL)"#,
            ),
            Alias::new("source_systems"),
        )
        .expr_as(
            Expr::cust(
                r#"ARRAY_AGG(DISTINCT "r"."provenance" ->> 'source') FILTER (WHERE "r"."provenance" ->> 'source' IS NOT NULL)"#,
            ),
            Alias::new("run_sources"),
        )
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::LeftJoin,
            data_streams::Entity,
            ds.clone(),
            Expr::col((ds, data_streams::Column::Id))
                .equals((r.clone(), readings::Column::StreamId)),
        )
        .and_where(
            Expr::col((r.clone(), readings::Column::ParameterId))
                .equals((p.clone(), parameters::Column::Id)),
        );
    if let Some(id) = site_id {
        observed.and_where(Expr::col((r, readings::Column::SiteId)).eq(id));
    }

    let empty_text_array = Expr::cust("ARRAY[]::text[]");
    let query = Query::select()
        .expr_as(
            Expr::col((p.clone(), parameters::Column::Id)),
            Alias::new("parameter_id"),
        )
        .expr_as(
            Expr::col((p.clone(), parameters::Column::Code)),
            Alias::new("parameter_code"),
        )
        .expr_as(
            Func::coalesce([
                Expr::col((cfg.clone(), Alias::new("sites_configured"))),
                Expr::val(0),
            ]),
            Alias::new("sites_configured"),
        )
        .expr_as(
            Func::coalesce([
                Expr::col((obs.clone(), Alias::new("reading_count"))),
                Expr::val(0),
            ]),
            Alias::new("reading_count"),
        )
        .column((obs.clone(), Alias::new("first_reading")))
        .column((obs.clone(), Alias::new("last_reading")))
        .expr_as(
            Func::coalesce([
                Expr::col((obs.clone(), Alias::new("source_systems"))),
                empty_text_array.clone(),
            ]),
            Alias::new("source_systems"),
        )
        .expr_as(
            Func::coalesce([
                Expr::col((obs.clone(), Alias::new("run_sources"))),
                empty_text_array,
            ]),
            Alias::new("run_sources"),
        )
        .from_as(parameters::Entity, p.clone())
        .join_lateral(JoinType::LeftJoin, configured, cfg, Expr::cust("TRUE"))
        .join_lateral(JoinType::LeftJoin, observed, obs, Expr::cust("TRUE"))
        .and_where(Expr::col((p.clone(), parameters::Column::Id)).is_in(parameter_ids.to_vec()))
        .order_by((p, parameters::Column::Code), Order::Asc)
        .to_owned();
    build(&query)
}

/// Coverage for a set of parameters, one query each over configuration, readings and provenance.
pub async fn coverage_for(
    state: &AppState,
    parameter_ids: &[Uuid],
    site_id: Option<Uuid>,
) -> AppResult<Vec<SlotCoverage>> {
    if parameter_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = state
        .db
        .query_all_raw(coverage_query(parameter_ids, site_id))
        .await?;

    let coverage = rows
        .iter()
        .map(|r| SlotCoverage::from_query_result(r, ""))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(coverage)
}

/// The open event-audit findings each calculation is carrying, and the visits they sit on.
///
/// Two grouped reads over the review queue: one counts the findings by kind, the other names the
/// visits, so a visit carrying three findings of one calculation counts once. A finding no
/// calculation raised (`tool` NULL) belongs to no row here.
pub async fn calculation_health(db: &DatabaseConnection) -> AppResult<Vec<CalculationHealth>> {
    #[derive(Debug, FromQueryResult)]
    struct KindCount {
        tool: String,
        kind: String,
        findings: i64,
    }
    #[derive(Debug, FromQueryResult)]
    struct VisitRow {
        tool: String,
    }

    let open = || {
        hold_model::of_kinds(
            Condition::all()
                .add(hold_model::Column::StreamId.is_null())
                .add(hold_model::Column::Tool.is_not_null())
                .add(hold_model::Column::Status.eq(HoldStatus::Pending.as_str())),
            &HoldKind::EVENT_AUDIT,
        )
    };

    let counts = hold_model::Entity::find()
        .filter(open())
        .select_only()
        .column(hold_model::Column::Tool)
        .column(hold_model::Column::Kind)
        .column_as(hold_model::Column::Id.count(), "findings")
        .group_by(hold_model::Column::Tool)
        .group_by(hold_model::Column::Kind)
        .into_model::<KindCount>()
        .all(db)
        .await?;

    // One row per (calculation, visit): the grouping is the distinct count, so the visits are
    // tallied here rather than counted twice by a finding that shares a slot.
    let visits = hold_model::Entity::find()
        .filter(open())
        .select_only()
        .column(hold_model::Column::Tool)
        .group_by(hold_model::Column::Tool)
        .group_by(hold_model::Column::SiteId)
        .group_by(hold_model::Column::GroupTime)
        .into_model::<VisitRow>()
        .all(db)
        .await?;

    fn entry<'a>(
        map: &'a mut HashMap<String, CalculationHealth>,
        tool: &str,
    ) -> &'a mut CalculationHealth {
        map.entry(tool.to_string())
            .or_insert_with(|| CalculationHealth {
                tool: tool.to_string(),
                stale_visits: 0,
                missing_outputs: 0,
                stale_outputs: 0,
                skipped_outputs: 0,
            })
    }

    let mut by_tool: HashMap<String, CalculationHealth> = HashMap::new();
    for row in &counts {
        let health = entry(&mut by_tool, &row.tool);
        match row.kind.as_str() {
            k if k == HoldKind::MissingOutput.as_str() => health.missing_outputs = row.findings,
            k if k == HoldKind::StaleOutput.as_str() => health.stale_outputs = row.findings,
            k if k == HoldKind::SkippedOutput.as_str() => health.skipped_outputs = row.findings,
            _ => {}
        }
    }
    for row in &visits {
        entry(&mut by_tool, &row.tool).stale_visits += 1;
    }
    let mut out: Vec<CalculationHealth> = by_tool.into_values().collect();
    out.sort_by(|a, b| a.tool.cmp(&b.tool));
    Ok(out)
}

/// Which subject the query names. One relation, four subjects (M126): the four are mutually
/// exclusive, because a closure answering two questions at once answers neither.
pub(super) fn closure_subject(query: &ClosureQuery) -> AppResult<Subject> {
    let named = [
        query.calibration_id.is_some(),
        query.site_parameter_id.is_some(),
        query.stream_id.is_some(),
        query.calculation.is_some(),
    ]
    .into_iter()
    .filter(|n| *n)
    .count();
    if named > 1 {
        return Err(AppError::BadRequest(
            "name one subject: calibration_id, site_parameter_id, stream_id or calculation"
                .to_string(),
        ));
    }
    if let Some(id) = query.calibration_id {
        return Ok(Subject::Calibration(id));
    }
    if let Some(id) = query.site_parameter_id {
        return Ok(Subject::Slot(id));
    }
    if let Some(stream_id) = query.stream_id {
        return Ok(Subject::Reading {
            stream_id,
            replicate_index: None,
        });
    }
    if let Some(name) = &query.calculation {
        return Ok(Subject::Calculation(name.clone()));
    }
    Ok(Subject::Parameters(parse_ids(
        query.parameter_ids.as_deref(),
    )?))
}

pub(super) struct CalculationRow {
    name: String,
    label: String,
    description: Option<String>,
    engine: Engine,
    active_version_id: Option<Uuid>,
    parameter_group_id: Option<Uuid>,
}

pub(super) async fn load_calculation<C: ConnectionTrait>(
    db: &C,
    script_id: Uuid,
) -> AppResult<Option<CalculationRow>> {
    let Some(row) = script::Entity::find_by_id(script_id).one(db).await? else {
        return Ok(None);
    };
    // An engine outside the vocabulary is a calculation this executor runs as a script, not a
    // decode failure.
    Ok(Some(CalculationRow {
        name: row.name,
        label: row.label,
        description: row.description,
        engine: Engine::parse(&row.engine).unwrap_or(Engine::Script),
        active_version_id: row.active_version_id,
        parameter_group_id: row.parameter_group_id,
    }))
}

/// Mint and activate a version for a formula calculation whose formula set has changed. A script
/// calculation is authored through the version routes and is left alone; an unchanged formula set
/// mints nothing.
pub async fn mint_formula_version<C: ConnectionTrait>(
    db: &C,
    script_id: Uuid,
    actor: Option<&str>,
) -> AppResult<Option<Uuid>> {
    let Some(calculation) = load_calculation(db, script_id).await? else {
        return Ok(None);
    };
    if calculation.engine != Engine::Formula {
        return Ok(None);
    }

    let formulas: Vec<PinnedFormula> = load_formulas(db, &[script_id])
        .await?
        .into_iter()
        .map(|(_, f)| f)
        .collect();
    let replicated = replicated_for(db, calculation.parameter_group_id).await?;
    let manifest = manifest_json(
        &calculation.label,
        calculation.description.as_deref(),
        &formulas,
        &replicated,
    )
    .map_err(AppError::Conflict)?;
    let body = render(&formulas).map_err(AppError::Conflict)?;
    let content_hash = canonical_hash(&serde_json::json!({
        "script": body,
        "manifest": manifest,
    }));

    check_manifest_codes_resolve(db, &calculation.name, &manifest).await?;

    if let Some(existing) = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM tool_script_versions \
              WHERE tool_script_id = $1 AND content_hash = $2",
            [script_id.into(), content_hash.clone().into()],
        ))
        .await?
    {
        let id: Uuid = existing.try_get("", "id")?;
        if calculation.active_version_id != Some(id) {
            activate(db, script_id, calculation.active_version_id, id, actor).await?;
        }
        return Ok(Some(id));
    }

    let version_id = Uuid::new_v4();
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO tool_script_versions \
             (id, tool_script_id, version_no, script, entry_function, manifest, content_hash, \
              created_by, validated_at) \
         SELECT $1, $2, COALESCE(MAX(version_no), 0) + 1, $3, 'formula', $4, $5, $6, now() \
           FROM tool_script_versions WHERE tool_script_id = $2",
        [
            version_id.into(),
            script_id.into(),
            body.into(),
            manifest.into(),
            content_hash.into(),
            actor.map(str::to_string).into(),
        ],
    ))
    .await?;
    activate(
        db,
        script_id,
        calculation.active_version_id,
        version_id,
        actor,
    )
    .await?;
    Ok(Some(version_id))
}

/// Re-mint every formula calculation whose formula set no longer matches its active version.
/// Idempotent, and cheap: the mint is a no-op when the content hash already matches, so this is
/// what the definition hooks call, including after a delete where the calculation the definition
/// belonged to is no longer readable from the row.
pub async fn mint_stale_formula_versions<C: ConnectionTrait>(
    db: &C,
    actor: Option<&str>,
) -> AppResult<()> {
    let ids: Vec<Uuid> = script::Entity::find()
        .select_only()
        .column(script::Column::Id)
        .filter(script::Column::Engine.eq("formula"))
        .into_tuple()
        .all(db)
        .await?;
    for id in ids {
        mint_formula_version(db, id, actor).await?;
    }
    Ok(())
}

/// The dedupe key an edit to one calculation audits under, so a burst of formula edits coalesces
/// into one audit the way a burst of constant edits does.
#[must_use]
pub fn audit_dedupe_key(name: &str) -> String {
    format!("event_audit:calculation:{name}")
}

/// A calculation's active version changed, so every output it has stored may disagree with what it
/// computes now. Nothing is rewritten: the edit enqueues the report-only `event_audit`, scoped to
/// the visits whose provenance names this calculation, and repair stays the scoped
/// `event_recompute` a person asks for. This is the policy `constants/operations.rs` already
/// applies to a constant edit, which is the same kind of change.
pub async fn audit_after_activation<C: ConnectionTrait>(db: &C, name: &str) {
    let key = audit_dedupe_key(name);
    if let Err(e) = crate::routes::private::reprocessing_jobs::service::enqueue(
        db,
        "event_audit",
        None,
        None,
        &serde_json::json!({ "calculation": name }),
        Some(&key),
    )
    .await
    {
        tracing::warn!(error = %e, calculation = %name, "failed to enqueue the calculation audit");
    }
}

pub(super) async fn activate<C: ConnectionTrait>(
    db: &C,
    script_id: Uuid,
    from: Option<Uuid>,
    to: Uuid,
    actor: Option<&str>,
) -> AppResult<()> {
    super::models::activation::ActiveModel {
        id: Set(Uuid::new_v4()),
        tool_script_id: Set(script_id),
        from_version_id: Set(from),
        to_version_id: Set(to),
        activated_by: Set(actor.map(str::to_string)),
        activated_at: Set(chrono::Utc::now()),
    }
    .insert(db)
    .await?;
    script::Entity::update_many()
        .col_expr(script::Column::ActiveVersionId, Expr::value(to))
        .col_expr(script::Column::UpdatedAt, Expr::current_timestamp())
        .filter(script::Column::Id.eq(script_id))
        .exec(db)
        .await?;
    if let Ok(Some(calculation)) = load_calculation(db, script_id).await {
        audit_after_activation(db, &calculation.name).await;
    }
    Ok(())
}

/// The catalog codes a manifest reads and writes, lowercased and deduplicated.
pub(super) fn manifest_codes(manifest: &serde_json::Value) -> (Vec<String>, Vec<String>) {
    let codes = |key: &str, field: &str| -> Vec<String> {
        manifest
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get(field).and_then(serde_json::Value::as_str))
                    .map(str::to_lowercase)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut inputs = codes("params", "parameter_code");
    inputs.extend(codes_of_event_inputs(manifest));
    inputs.sort();
    inputs.dedup();
    (inputs, codes("outputs", "suggested_parameter_code"))
}

/// `LOWER(code) = ANY($1)`, matched against codes the caller already lowered.
fn lowered_code_in(codes: &[String]) -> sea_orm::sea_query::SimpleExpr {
    use sea_orm::sea_query::{ExprTrait, Func};
    Expr::expr(Func::lower(Expr::col(parameters::Column::Code))).is_in(codes.to_vec())
}

/// The catalog ids of the given lowercased codes, in no particular order.
pub(super) async fn parameter_ids_of_codes<C: ConnectionTrait>(
    db: &C,
    codes: Vec<String>,
) -> AppResult<Vec<Uuid>> {
    Ok(parameters::Entity::find()
        .select_only()
        .column(parameters::Column::Id)
        .filter(lowered_code_in(&codes))
        .into_tuple()
        .all(db)
        .await?)
}

/// Catalog ids by lowercased code, for the codes asked for. A code the catalog does not hold is
/// simply absent, which is what the caller has to decide about.
pub(super) async fn ids_by_code<C: ConnectionTrait>(
    db: &C,
    codes: &[String],
) -> AppResult<std::collections::HashMap<String, Uuid>> {
    let mut wanted: Vec<String> = codes.to_vec();
    wanted.sort();
    wanted.dedup();
    let rows = parameters::Entity::find()
        .filter(lowered_code_in(&wanted))
        .all(db)
        .await?;
    let mut by_code = std::collections::HashMap::new();
    for row in rows {
        by_code.insert(row.code.to_lowercase(), row.id);
    }
    Ok(by_code)
}

/// The calculations bound to a group, as the reshape rules read them: name, inputs and outputs
/// resolved from each calculation's *active* version. A calculation with no active version
/// produces nothing yet and is not one.
pub async fn calculations_of_group<C: ConnectionTrait>(
    db: &C,
    group_id: Uuid,
) -> AppResult<Vec<rules::Calculation>> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT s.name, v.manifest FROM tool_scripts s                JOIN tool_script_versions v ON v.id = s.active_version_id               WHERE s.parameter_group_id = $1",
            [group_id.into()],
        ))
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let ActiveManifestRow { name, manifest } = ActiveManifestRow::from_query_result(row, "")?;
        let (input_codes, output_codes) = manifest_codes(&manifest);
        let mut all = input_codes.clone();
        all.extend(output_codes.clone());
        let by_code = ids_by_code(db, &all).await?;
        out.push(rules::Calculation {
            group_id,
            name,
            inputs: input_codes
                .iter()
                .filter_map(|c| by_code.get(c).copied())
                .collect(),
            outputs: output_codes
                .iter()
                .filter_map(|c| by_code.get(c).copied())
                .collect(),
        });
    }
    Ok(out)
}

/// Every parameter a manifest names exists in the catalog.
///
/// It used to also require each one to be a member of the calculation's own group in the role the
/// member declared. Q135 retired that: roles are read off the calculations and a group is a way to
/// list many parameters together, so a calculation reads any catalog parameter. What is left is
/// that a code it names must resolve to one.
pub async fn check_manifest_codes_resolve<C: ConnectionTrait>(
    db: &C,
    name: &str,
    manifest: &serde_json::Value,
) -> AppResult<()> {
    let (input_codes, output_codes) = manifest_codes(manifest);
    if input_codes.is_empty() && output_codes.is_empty() {
        return Ok(());
    }

    let mut wanted: Vec<String> = input_codes
        .iter()
        .chain(output_codes.iter())
        .cloned()
        .collect();
    wanted.sort();
    wanted.dedup();
    let by_code = ids_by_code(db, &wanted).await?;
    for code in &wanted {
        if !by_code.contains_key(code) {
            return Err(AppError::BadRequest(format!(
                "calculation {name} names parameter {code}, which is not in the catalog"
            )));
        }
    }

    Ok(())
}

pub(super) fn codes_of_event_inputs(manifest: &serde_json::Value) -> Vec<String> {
    manifest
        .get("event_inputs")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.get("parameter_code")
                        .and_then(serde_json::Value::as_str)
                })
                .map(str::to_lowercase)
                .collect()
        })
        .unwrap_or_default()
}

/// The three list queries the calculation rules make, as their SELECTs return them.
#[derive(FromQueryResult)]
pub(super) struct ActiveManifestRow {
    name: String,
    manifest: serde_json::Value,
}

/// A tool's name is a path segment (`/tools/{name}/calculate`) and a manifest key, so it is
/// lower-cased and refused unless it is `[a-z0-9_]`.
pub(crate) fn normalise_name(name: &str) -> Result<String, ApiError> {
    let name = name.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(ApiError::bad_request(
            "tool name must be non-empty [a-z0-9_]".to_string(),
        ));
    }
    Ok(name.to_lowercase())
}

/// `script` or `formula`, or nothing at all.
pub(crate) fn check_engine(engine: &str) -> Result<(), ApiError> {
    if Engine::parse(engine).is_none() {
        return Err(ApiError::bad_request(format!(
            "engine {engine} is not script or formula"
        )));
    }
    Ok(())
}

/// One calculation's version count and the number of the version that is live.
#[derive(FromQueryResult)]
pub(super) struct VersionCounts {
    id: Uuid,
    active_version_no: Option<i32>,
    version_count: i64,
}

/// The version count and the live version's number, for a page of calculations.
pub(super) async fn counts<C: ConnectionTrait>(
    db: &C,
    ids: &[Uuid],
) -> Result<Vec<VersionCounts>, ApiError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT s.id, av.version_no AS active_version_no,
                     (SELECT count(*) FROM tool_script_versions v
                       WHERE v.tool_script_id = s.id) AS version_count
                FROM tool_scripts s
                LEFT JOIN tool_script_versions av ON av.id = s.active_version_id
               WHERE s.id = ANY($1)",
            [ids.to_vec().into()],
        ))
        .await
        .map_err(ApiError::database)?;
    rows.iter()
        .map(|row| VersionCounts::from_query_result(row, "").map_err(ApiError::database))
        .collect()
}

impl CRUDOperations for ToolScriptOperations {
    type Resource = ToolScript;

    /// The engine is checked before the insert; the name is normalised in `perform_create`, which
    /// takes the request by value. `before_create` is handed it by reference and cannot correct it.
    async fn before_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        _db: &C,
        data: &<ToolScript as crudcrate::CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        normalise_name(&data.name)?;
        if let Some(engine) = data.engine.as_deref() {
            check_engine(engine)?;
        }
        Ok(())
    }

    /// The insert with the name normalised, and the unique violation read as the conflict it is.
    async fn perform_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        mut data: <ToolScript as crudcrate::CRUDResource>::CreateModel,
    ) -> Result<ToolScript, ApiError> {
        data.name = normalise_name(&data.name)?;
        let name = data.name.clone();
        let active: <ToolScript as crudcrate::CRUDResource>::ActiveModelType = data.into();
        active.insert(db).await.map(ToolScript::from).map_err(|e| {
            if e.to_string().contains("idx_tool_scripts_name") {
                ApiError::conflict(format!("a tool named '{name}' already exists"))
            } else {
                ApiError::database(e)
            }
        })
    }

    /// The name is `exclude(update)`, so an update can only reach the engine.
    async fn before_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        _db: &C,
        _id: Uuid,
        data: &<ToolScript as crudcrate::CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        if let Some(Some(engine)) = data.engine.as_ref() {
            check_engine(engine)?;
        }
        Ok(())
    }

    /// The version history, newest first, and which of them is live.
    async fn after_get_one<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut ToolScript,
    ) -> Result<(), ApiError> {
        let versions = super::models::version::Entity::find()
            .filter(super::models::version::Column::ToolScriptId.eq(entity.id))
            .order_by_desc(super::models::version::Column::VersionNo)
            .all(db)
            .await
            .map_err(ApiError::database)?;
        entity.versions = versions
            .into_iter()
            .map(|m| {
                let mut v = ToolScriptVersionList::from(ToolScriptVersion::from(m));
                v.active = entity.active_version_id == Some(v.id);
                v
            })
            .collect();
        entity.version_count = entity.versions.len() as i64;
        entity.active_version_no = entity
            .versions
            .iter()
            .find(|v| v.active)
            .map(|v| v.version_no);
        Ok(())
    }

    async fn after_get_all<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entities: &mut Vec<<ToolScript as crudcrate::CRUDResource>::ListModel>,
    ) -> Result<(), ApiError> {
        let ids: Vec<Uuid> = entities.iter().map(|e| e.id).collect();
        let counted = counts(db, &ids).await?;
        for entity in entities.iter_mut() {
            if let Some(counts) = counted.iter().find(|c| c.id == entity.id) {
                entity.active_version_no = counts.active_version_no;
                entity.version_count = counts.version_count;
            }
        }
        Ok(())
    }
}

/// A page big enough for a catalogue of calculations; the surface is a dozen rows, not a feed.
pub(super) const LIST_LIMIT: u64 = 500;

/// Packages a script may load or reach into with `::`. Mirrors the runner image's installed set
/// plus the runner's own package; anything else fails at run time anyway, the lint just says so
/// earlier.
pub(super) const LIBRARY_WHITELIST: &[&str] = &[
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
pub(super) const PACKAGE_LOADERS: &[&str] = &[
    "library",
    "require",
    "requireNamespace",
    "loadNamespace",
    "attachNamespace",
];

/// Calls that resolve a function from a name given as a string, which is how a forbidden call is
/// reached without ever being written as one.
pub(super) const NAME_RESOLVERS: &[&str] = &[
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
pub(super) const FUNCTION_ARGS: &[&str] = &[
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
pub(super) const FORBIDDEN: &[(&str, &str)] = &[
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
pub(super) const PARTIAL_FILE_ARGS: &[(&str, &str)] = &[("cat", "file")];

/// A manifest finding, carried in the same shape as a script finding so a caller renders one list.
pub(super) fn manifest_finding(message: String) -> LintFinding {
    LintFinding { line: 0, message }
}

pub(super) fn forbidden_reason(name: &str) -> Option<&'static str> {
    FORBIDDEN
        .iter()
        .find(|(forbidden, _)| *forbidden == name)
        .map(|(_, why)| *why)
}

pub(super) fn line_of(line: i64) -> usize {
    usize::try_from(line).unwrap_or(1).max(1)
}

pub(super) fn refusal(line: i64, name: &str, why: &str) -> LintFinding {
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
pub(super) fn push_name_findings(findings: &mut Vec<LintFinding>, reference: &ScannedName) {
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
pub(super) fn findings_from_scan(scan: &ScriptScan) -> Vec<LintFinding> {
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
pub(super) async fn lint_script(state: &AppState, script: &str) -> AppResult<Vec<LintFinding>> {
    let scan = scan_script(state, script).await?;
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

/// One calculation with its version history, through the entity's own read path so the counts and
/// the `active` flag come from one place.
pub(super) async fn load_script(state: &AppState, id: Uuid) -> AppResult<ToolScript> {
    <ToolScript as CRUDResource>::get_one(&state.db, id)
        .await
        .map_err(|_| AppError::NotFound(format!("tool script {id} not found")))
}

/// One version of one calculation. The script id is part of the key: a version id belonging to
/// another calculation is not found rather than served under the wrong parent.
pub(super) async fn load_version(
    state: &AppState,
    script_id: Uuid,
    vid: Uuid,
) -> AppResult<ToolScriptVersion> {
    super::models::version::Entity::find_by_id(vid)
        .filter(super::models::version::Column::ToolScriptId.eq(script_id))
        .one(&state.db)
        .await?
        .map(ToolScriptVersion::from)
        .ok_or_else(|| AppError::NotFound(format!("version {vid} not found")))
}

pub(super) fn as_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::Array(a) if a.len() == 1 => a[0].as_f64(),
        _ => None,
    }
}

/// Tolerant structural equality: numbers compare within `tol * max(|expected|, 1)` at any
/// nesting depth, and a result object may carry keys the expectation does not name.
pub(super) fn matches_expected(
    got: &serde_json::Value,
    expected: &serde_json::Value,
    tol: f64,
) -> bool {
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
pub(super) fn version_as_tool(
    script: &ToolScript,
    version: &ToolScriptVersion,
) -> AppResult<ActiveTool> {
    let manifest = parse_manifest(&version.manifest)
        .map_err(|e| AppError::BadRequest(format!("invalid manifest: {e}")))?;
    Ok(ActiveTool {
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
        engine: Engine::Script,
        parameter_group_id: None,
        formulas: Vec::new(),
    })
}

/// A case's request body: its inputs plus its curves, which the manifest names as fields of the
/// same body. A curve given as coefficients resolves without the database, which is what keeps a
/// case reproducible.
pub(super) fn case_body(case: &serde_json::Value) -> serde_json::Value {
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
pub(super) async fn run_stored_cases(
    state: &AppState,
    id: Uuid,
    version: &ToolScriptVersion,
) -> AppResult<ValidateResponse> {
    let tool = version_as_tool(&load_script(state, id).await?, version)?;
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
        let outcome = run_tool_body(state, &tool, &body, constants, MissingConstant::Refuse).await;

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
    version_entity::Entity::update_many()
        .col_expr(
            version_entity::Column::ValidatedAt,
            Expr::value(validated_at),
        )
        .filter(version_entity::Column::Id.eq(version.id))
        .exec(&state.db)
        .await?;

    Ok(ValidateResponse {
        passed: all_passed,
        cases: results,
        validated_at,
    })
}

#[cfg(test)]
#[path = "tests/calculations.rs"]
mod calculations_tests;

#[cfg(test)]
#[path = "tests/closure.rs"]
mod closure_tests;

#[cfg(test)]
#[path = "tests/cnet_formula_sets.rs"]
mod cnet_formula_sets_tests;

#[cfg(test)]
#[path = "tests/engine.rs"]
mod engine_tests;

#[cfg(test)]
#[path = "tests/formula.rs"]
mod formula_tests;

#[cfg(test)]
#[path = "tests/hash.rs"]
mod hash_tests;

#[cfg(test)]
#[path = "tests/runner.rs"]
mod runner_tests;

#[cfg(test)]
#[path = "tests/script_operations.rs"]
mod script_operations_tests;

#[cfg(test)]
#[path = "tests/scripts.rs"]
mod scripts_tests;
