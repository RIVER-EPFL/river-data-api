use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, Set, Statement, TransactionTrait,
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use super::models::definition::CalculationFormula;
use super::models::source;
use crate::routes::private::constants;
use crate::routes::private::parameters;
use crate::routes::private::projects;
use crate::routes::private::readings;
use crate::routes::private::sensor_calibrations::service::{DerivedSlot, SlotPass};
use crate::routes::private::site_parameters;
use crate::routes::private::sites;
use crate::routes::private::sync::hold_model;
use crate::routes::private::sync::models::{HoldKind, HoldStatus};
use crate::routes::private::sync::service::{Hold, HoldKey, upsert_hold};
use crate::routes::private::tools::service::{CURVE_VARIABLES, free_identifiers};

/// A formula's content hash: sha256 over the text itself, so one text is one version however it
/// was saved.
fn formula_hash(formula: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(formula.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

/// Mint a version of a standalone definition's formula, unless the newest one already holds that
/// text.
///
/// An edit is a new calculation rather than a correction of the old one (Q89), so the text a
/// stored value was made with stays recoverable. A definition attached to a calculation is
/// versioned by `tool_script_versions` instead, minted once per save of its formula set, so this
/// covers only the standalone kind the per-reading engine serves.
async fn mint_derived_version<C: ConnectionTrait>(
    db: &C,
    definition_id: Uuid,
    formula: &str,
    actor: Option<&str>,
) -> Result<(), ApiError> {
    let hash = formula_hash(formula);
    let newest = super::models::version::Entity::find()
        .filter(super::models::version::Column::DefinitionId.eq(definition_id))
        .order_by_desc(super::models::version::Column::VersionNo)
        .one(db)
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
    // A re-save that changed nothing else leaves the formula where it is, so the newest version
    // already carrying this text is the version.
    if newest.as_ref().is_some_and(|v| v.content_hash == hash) {
        return Ok(());
    }
    super::models::version::ActiveModel {
        id: Set(Uuid::new_v4()),
        definition_id: Set(definition_id),
        version_no: Set(newest.map_or(1, |v| v.version_no + 1)),
        formula: Set(formula.to_string()),
        content_hash: Set(hash),
        created_by: Set(actor.map(str::to_string)),
        ..Default::default()
    }
    .insert(db)
    .await
    .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
    Ok(())
}

/// Whether this definition is the standalone kind, ie. not attached to a calculation.
async fn is_standalone<C: ConnectionTrait>(db: &C, definition_id: Uuid) -> Result<bool, ApiError> {
    // No row is a definition that does not exist.
    let row = super::models::definition::Entity::find_by_id(definition_id)
        .one(db)
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
    // A shared step also belongs to no calculation (Q156), and is not the continuous kind: it
    // mints no parameter and the derived job has nothing of it to serve.
    Ok(row.is_some_and(|d| d.tool_script_id.is_none() && !d.intermediate))
}

/// A reducer takes a replicate family, and a continuous definition reads one value per instant, so
/// there is nothing for it to reduce. It is refused at the save rather than left to fail on every
/// reading the derived job computes.
pub(crate) fn refuse_reducers(formula: &str) -> Result<(), ApiError> {
    match crate::routes::private::tools::service::reducer_calls(formula).first() {
        Some(call) => Err(ApiError::bad_request(format!(
            "'{}' reduces a replicate family, which a continuous calculation does not have",
            call.call
        ))),
        None => Ok(()),
    }
}

pub(crate) fn validate_formula(formula: &str) -> Result<(), ApiError> {
    formula
        .parse::<meval::Expr>()
        .map_err(|e| ApiError::bad_request(format!("Invalid formula: {e}")))?;
    Ok(())
}

/// What a formula's identifiers resolve to.
#[derive(Default)]
pub(crate) struct ResolvedSources {
    /// `(variable_name, parameter_id)`: read from the event's stored readings. The variable is
    /// the parameter's code, which is how it resolved.
    pub(crate) parameters: Vec<(String, Uuid)>,
    /// `(variable_name, site_property)`: read from the site's own row.
    pub(crate) site_properties: Vec<(String, String)>,
}

/// Resolve each formula variable, with strict validation.
///
/// Most specific first: a catalog parameter, then a `constants` row, then a column of `sites`.
/// An identifier naming a constant is not a variable at all: it resolves to the same value at
/// every site and instant, so it is left out of the sources and bound at evaluation from the
/// constants table, exactly as the script engine binds a declared constant. A column of `sites`
/// is a property of the station rather than a measurement, so it is recorded as a site source and
/// resolved from the site row at calculate time, never asked for at the visit.
pub(crate) async fn resolve_variables<C: ConnectionTrait>(
    db: &C,
    formula: &str,
    context: &FormulaContext<'_>,
) -> Result<ResolvedSources, ApiError> {
    let steps = steps_of(db, context).await?;
    let names: Vec<String> = variables_of(formula, context.curve_slot)?
        .into_iter()
        .filter(|name| !steps.iter().any(|step| step == name))
        .collect();
    resolve_identifiers(db, &names).await
}

// --- Shared steps (Q156) ---

/// What declaring a shared step does to the step's own row.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Promotion {
    /// The step is owned by no calculation already: only the new declaration is written.
    AlreadyShared,
    /// The step was one calculation's own: it is released, and that calculation keeps reading it
    /// through a declaration of its own.
    Release { previous_owner: Uuid },
}

/// Whether a calculation may declare this step, and what the declaration does to its ownership.
///
/// A step the declaring calculation already owns is refused: releasing it would take the step out
/// of the set that computes it and put it back through the declaration, which is a rename of the
/// same state and reads as a mistake rather than a sharing.
pub(crate) fn promotion(owner: Option<Uuid>, declaring: Uuid) -> Result<Promotion, String> {
    match owner {
        None => Ok(Promotion::AlreadyShared),
        Some(owner) if owner == declaring => Err(
            "This calculation already computes that step; a declaration is for a step another \
             calculation wrote"
                .to_string(),
        ),
        Some(previous_owner) => Ok(Promotion::Release { previous_owner }),
    }
}

/// The steps a calculation reads through a declaration, as the formula rows themselves.
pub(crate) async fn declared_steps<C: ConnectionTrait>(
    db: &C,
    tool_script_id: Uuid,
) -> Result<Vec<super::models::definition::Model>, ApiError> {
    let declared = super::models::shared_step::Entity::find()
        .filter(super::models::shared_step::Column::ToolScriptId.eq(tool_script_id))
        .all(db)
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
    if declared.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = declared.into_iter().map(|row| row.formula_id).collect();
    super::models::definition::Entity::find()
        .filter(super::models::definition::Column::Id.is_in(ids))
        .order_by_asc(super::models::definition::Column::Ordinal)
        .order_by_asc(super::models::definition::Column::Code)
        .all(db)
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))
}

/// Everything that reads one step: the calculation that owns it, every calculation that declares
/// it, and inside each, the formulas whose text names the step's code (M208). A step mints no
/// catalog parameter, so it cannot be asked about through the parameter graph.
pub async fn dependents_of_step<C: ConnectionTrait>(
    db: &C,
    formula_id: Uuid,
) -> Result<super::models::StepDependents, ApiError> {
    let step = super::models::definition::Entity::find_by_id(formula_id)
        .one(db)
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?
        .ok_or_else(|| ApiError::not_found("No formula carries that id".to_string(), None))?;

    let declared = super::models::shared_step::Entity::find()
        .filter(super::models::shared_step::Column::FormulaId.eq(formula_id))
        .all(db)
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;

    let mut script_ids: Vec<Uuid> = step.tool_script_id.into_iter().collect();
    for declaration in &declared {
        if !script_ids.contains(&declaration.tool_script_id) {
            script_ids.push(declaration.tool_script_id);
        }
    }

    let scripts = crate::routes::private::tools::models::script::Entity::find()
        .filter(crate::routes::private::tools::models::script::Column::Id.is_in(script_ids.clone()))
        .all(db)
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
    let formulas = crate::routes::private::tools::service::load_formulas(db, &script_ids)
        .await
        .map_err(|e| ApiError::internal(e.to_string(), None))?;

    let mut calculations = Vec::with_capacity(script_ids.len());
    for script_id in script_ids {
        let Some(script) = scripts.iter().find(|s| s.id == script_id) else {
            continue;
        };
        let readers = formulas
            .iter()
            .filter(|(owner, formula)| {
                *owner == script_id
                    && formula.code != step.code
                    && free_identifiers(&formula.formula).contains(&step.code)
            })
            .map(|(_, formula)| super::models::StepReader {
                code: formula.code.clone(),
                formula: formula.formula.clone(),
            })
            .collect();
        calculations.push(super::models::StepDependent {
            tool_script_id: script_id,
            name: script.name.clone(),
            label: script.label.clone(),
            owns: step.tool_script_id == Some(script_id),
            formulas: readers,
        });
    }

    Ok(super::models::StepDependents {
        formula_id,
        code: step.code,
        shared: step.tool_script_id.is_none(),
        calculations,
    })
}

pub struct SharedStepOperations;

impl CRUDOperations for SharedStepOperations {
    type Resource = super::models::shared_step::CalculationSharedStep;

    /// A declaration names a step: a formula that computes an intermediate value and is not some
    /// calculation's output. What it does to the step's ownership is decided here and applied
    /// after the row lands.
    async fn before_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        data: &<Self::Resource as CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        let step = super::models::definition::Entity::find_by_id(data.formula_id)
            .one(db)
            .await
            .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?
            .ok_or_else(|| ApiError::bad_request("No formula carries that id".to_string()))?;
        if !step.intermediate {
            return Err(ApiError::bad_request(format!(
                "Formula '{}' is an output, not a step; a calculation reads an output as a \
                 parameter",
                step.code
            )));
        }
        promotion(step.tool_script_id, data.tool_script_id).map_err(ApiError::bad_request)?;
        Ok(())
    }

    /// The step becomes nobody's, and the calculation that wrote it keeps reading it the same way
    /// the declaring one now does.
    async fn after_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut Self::Resource,
    ) -> Result<(), ApiError> {
        let step = super::models::definition::Entity::find_by_id(entity.formula_id)
            .one(db)
            .await
            .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?
            .ok_or_else(|| ApiError::bad_request("No formula carries that id".to_string()))?;
        let Promotion::Release { previous_owner } =
            promotion(step.tool_script_id, entity.tool_script_id).map_err(ApiError::bad_request)?
        else {
            return Ok(());
        };
        release_step(db, entity.formula_id).await?;
        declare_step(db, previous_owner, entity.formula_id).await?;
        // A declaration changes the shape of both calculations' pinned sets, and it is the whole
        // act, so each is minted once here rather than by a sweep over every formula calculation.
        for calculation in [previous_owner, entity.tool_script_id] {
            crate::routes::private::tools::service::mint_formula_version(db, calculation, None)
                .await
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
        }
        Ok(())
    }
}

/// Take a step out of the calculation that wrote it: a shared step is owned by none.
async fn release_step<C: ConnectionTrait>(db: &C, formula_id: Uuid) -> Result<(), ApiError> {
    super::models::definition::Entity::update_many()
        .col_expr(
            super::models::definition::Column::ToolScriptId,
            sea_orm::sea_query::Expr::value(Option::<Uuid>::None),
        )
        .filter(super::models::definition::Column::Id.eq(formula_id))
        .exec(db)
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
    Ok(())
}

/// One calculation's reading of a shared step, written once however often it is asked for.
async fn declare_step<C: ConnectionTrait>(
    db: &C,
    tool_script_id: Uuid,
    formula_id: Uuid,
) -> Result<(), ApiError> {
    super::models::shared_step::Entity::insert(super::models::shared_step::ActiveModel {
        id: Set(Uuid::new_v4()),
        tool_script_id: Set(tool_script_id),
        formula_id: Set(formula_id),
        ..Default::default()
    })
    .on_conflict(
        sea_orm::sea_query::OnConflict::columns([
            super::models::shared_step::Column::ToolScriptId,
            super::models::shared_step::Column::FormulaId,
        ])
        .do_nothing()
        .to_owned(),
    )
    .try_insert()
    .exec(db)
    .await
    .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
    Ok(())
}

/// What a formula is resolved in: the calculation whose earlier steps it may read, and the curve
/// slot that binds its coefficients.
pub(crate) struct FormulaContext<'a> {
    pub tool_script_id: Option<Uuid>,
    pub code: &'a str,
    pub curve_slot: Option<&'a str>,
}

/// The codes of the steps this formula may read: the calculation's own intermediates and the
/// shared steps it declares. A step stores nothing and mints no parameter, so its code names no
/// reading: it reaches the formulas after it from the run, and recording it as a source would send
/// the evaluation looking for a value the visit never holds.
async fn steps_of<C: ConnectionTrait>(
    db: &C,
    context: &FormulaContext<'_>,
) -> Result<Vec<String>, ApiError> {
    let Some(tool_script_id) = context.tool_script_id else {
        return Ok(Vec::new());
    };
    let mut codes: Vec<String> = super::models::definition::Entity::find()
        .filter(super::models::definition::Column::ToolScriptId.eq(tool_script_id))
        .filter(super::models::definition::Column::Intermediate.eq(true))
        .filter(super::models::definition::Column::Code.ne(context.code))
        .all(db)
        .await
        .map(|rows| rows.into_iter().map(|row| row.code).collect())
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
    for step in declared_steps(db, tool_script_id).await? {
        if step.code != context.code && !codes.contains(&step.code) {
            codes.push(step.code);
        }
    }
    Ok(codes)
}

/// The identifiers a formula reads, minus the two its curve slot binds.
///
/// `curve_slope` and `curve_intercept` are supplied at evaluation from the slot's curve
/// ([`crate::routes::private::tools::service::CURVE_VARIABLES`]), so they name no parameter,
/// constant or site column. A formula reading one with no slot to bind it is refused by name: it
/// could never be evaluated.
pub(crate) fn variables_of(
    formula: &str,
    curve_slot: Option<&str>,
) -> Result<Vec<String>, ApiError> {
    let bound = curve_slot.is_some_and(|slot| !slot.trim().is_empty());
    let mut names = Vec::new();
    for name in free_identifiers(formula) {
        if !CURVE_VARIABLES.contains(&name.as_str()) {
            names.push(name);
            continue;
        }
        if !bound {
            return Err(ApiError::bad_request(format!(
                "Formula variable '{name}' is a curve coefficient; the formula needs a curve slot                  to bind it"
            )));
        }
    }
    Ok(names)
}

/// [`resolve_variables`] over identifiers already taken from a formula, for a caller that has
/// set some aside (a draft set's own codes, which no parameter carries yet).
pub(crate) async fn resolve_identifiers<C: ConnectionTrait>(
    db: &C,
    var_names: &[String],
) -> Result<ResolvedSources, ApiError> {
    let mut resolved = ResolvedSources::default();
    let mut site_columns: Option<Vec<String>> = None;

    for var_name in var_names {
        let row = parameters::Entity::find()
            .filter(parameters::Column::Code.eq(var_name.as_str()))
            .one(db)
            .await
            .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;

        if let Some(parameter) = row {
            resolved.parameters.push((var_name.clone(), parameter.id));
        } else if !names_a_constant(db, var_name).await? {
            let columns = match &site_columns {
                Some(columns) => columns,
                None => site_columns.insert(site_columns_of(db).await?),
            };
            if columns.contains(var_name) {
                resolved
                    .site_properties
                    .push((var_name.clone(), var_name.clone()));
            } else {
                return Err(ApiError::bad_request(format!(
                    "Formula variable '{var_name}' does not match any parameter, constant or site \
                     property"
                )));
            }
        }
    }

    Ok(resolved)
}

/// The columns of the `sites` row, which is what a site source may name (D13: any column is
/// resolvable, and the kind check at calculate time is what refuses a text one in a number input).
async fn site_columns_of<C: ConnectionTrait>(db: &C) -> Result<Vec<String>, ApiError> {
    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'sites'",
        ))
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
    // A skipped column is a site source refused for naming something that exists, so a decode
    // failure is an error rather than a shorter list.
    rows.iter()
        .map(|r| {
            r.try_get::<String>("", "column_name")
                .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))
        })
        .collect()
}

/// Whether the constants table holds this name.
async fn names_a_constant<C: ConnectionTrait>(db: &C, name: &str) -> Result<bool, ApiError> {
    let row = constants::models::Entity::find()
        .filter(constants::models::Column::Name.eq(name))
        .one(db)
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
    Ok(row.is_some())
}

/// What the derived definitions produce and what they read, loaded once so the cycle and depth
/// guards are a walk over data rather than a query per node.
///
/// A definition is found by `output_parameter_id`, the column that says what it produces. Its own
/// `code` names the formula, and the two are routinely spelled differently: `ensure_output_parameter`
/// creates the output parameter rather than requiring them to agree.
#[derive(Default)]
struct DerivedGraph {
    /// Output parameter id to the definition producing it.
    definition_of: std::collections::HashMap<Uuid, Uuid>,
    /// Definition id to the parameter ids its formula reads.
    sources_of: std::collections::HashMap<Uuid, Vec<Uuid>>,
}

impl DerivedGraph {
    async fn load<C: ConnectionTrait>(db: &C) -> Result<Self, ApiError> {
        let mut graph = Self::default();
        let definitions = super::models::definition::Entity::find()
            .filter(super::models::definition::Column::OutputParameterId.is_not_null())
            .all(db)
            .await
            .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
        for definition in definitions {
            if let Some(output_parameter_id) = definition.output_parameter_id {
                graph
                    .definition_of
                    .insert(output_parameter_id, definition.id);
            }
        }

        let sources = source::Entity::find()
            .all(db)
            .await
            .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
        for source in sources {
            if let Some(parameter_id) = source.parameter_id {
                graph
                    .sources_of
                    .entry(source.derived_definition_id)
                    .or_default()
                    .push(parameter_id);
            }
        }
        Ok(graph)
    }
}

/// Refuse a set of formula variables that would make the definition producing `output_parameter_id`
/// part of a cycle. `output_parameter_id` is None for a definition whose output parameter does not
/// exist yet, which nothing can read and so cannot close a cycle.
///
/// The question is the one every engine asks of its own graph, so it is asked here through
/// `common::dependency` rather than walked again: build the graph this save would create, and
/// order it. Depth is not a question any more (Q96): a chain that orders is runnable however deep
/// it is.
fn validate_dependency_chain(
    graph: &DerivedGraph,
    output_parameter_id: Option<Uuid>,
    resolved_params: &[(String, Uuid)],
) -> Result<(), String> {
    let Some(output) = output_parameter_id else {
        return Ok(());
    };
    if let Some((var_name, _)) = resolved_params.iter().find(|(_, id)| *id == output) {
        return Err(format!(
            "Circular dependency detected: variable '{var_name}' references its own output \
             parameter"
        ));
    }

    // Every parameter the prospective graph mentions, as an index; an edge runs from the parameter
    // a definition reads to the parameter it produces.
    let mut index: HashMap<Uuid, usize> = HashMap::new();
    let mut name: Vec<Uuid> = Vec::new();
    let slot = |id: Uuid, index: &mut HashMap<Uuid, usize>, name: &mut Vec<Uuid>| -> usize {
        *index.entry(id).or_insert_with(|| {
            name.push(id);
            name.len() - 1
        })
    };
    let mut edges: Vec<(usize, usize)> = Vec::new();
    for (produced, definition_id) in &graph.definition_of {
        let to = slot(*produced, &mut index, &mut name);
        for source in graph.sources_of.get(definition_id).into_iter().flatten() {
            let from = slot(*source, &mut index, &mut name);
            edges.push((to, from));
        }
    }
    // The save's own edges, which is what makes this a question about the graph it would create
    // rather than the one that is stored.
    let to = slot(output, &mut index, &mut name);
    for (_, parameter_id) in resolved_params {
        let from = slot(*parameter_id, &mut index, &mut name);
        edges.push((to, from));
    }

    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); name.len()];
    for (to, from) in edges {
        deps[to].push(from);
    }
    let Some(cycle) = crate::common::dependency::cycle(&deps) else {
        return Ok(());
    };
    // Only the variables of this save can be named here; the rest of the cycle is stored graph.
    let named: Vec<&str> = resolved_params
        .iter()
        .filter(|(_, id)| index.get(id).is_some_and(|i| cycle.contains(i)))
        .map(|(var, _)| var.as_str())
        .collect();
    Err(if named.is_empty() {
        "Circular dependency detected: this definition would close a loop in the derived chain"
            .to_string()
    } else {
        format!(
            "Circular dependency detected: variable '{}' is derived from this definition's own \
             output parameter",
            named.join("', '")
        )
    })
}

/// Load the graph and validate against it, the shape the CRUD hooks use.
async fn validate_against_stored_graph<C: ConnectionTrait>(
    db: &C,
    output_parameter_id: Option<Uuid>,
    resolved_params: &[(String, Uuid)],
) -> Result<(), ApiError> {
    let graph = DerivedGraph::load(db).await?;
    validate_dependency_chain(&graph, output_parameter_id, resolved_params)
        .map_err(ApiError::bad_request)
}

/// The catalog parameter a code already names, if any.
async fn existing_parameter_id<C: ConnectionTrait>(
    db: &C,
    code: &str,
) -> Result<Option<Uuid>, ApiError> {
    let row = parameters::Entity::find()
        .filter(code_matches(code))
        .one(db)
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
    Ok(row.map(|parameter| parameter.id))
}

/// `LOWER(code) = LOWER($1)`, the shape of the catalog's unique index on the code.
fn code_matches(code: &str) -> sea_orm::sea_query::SimpleExpr {
    use sea_orm::sea_query::{Expr, ExprTrait, Func};
    Expr::expr(Func::lower(Expr::col(parameters::Column::Code))).eq(code.to_lowercase())
}

/// The parameter a stored definition produces, and its formula.
async fn stored_definition<C: ConnectionTrait>(
    db: &C,
    id: Uuid,
) -> Result<super::models::definition::Model, ApiError> {
    super::models::definition::Entity::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?
        .ok_or_else(|| ApiError::not_found("Derived parameter definition", None))
}

/// Delete existing sources and insert new ones for a derived definition.
async fn sync_sources<C: ConnectionTrait>(
    db: &C,
    definition_id: Uuid,
    resolved: &ResolvedSources,
) -> Result<(), ApiError> {
    let resolved_params = &resolved.parameters;
    // Delete existing rows
    delete_sources(db, definition_id)
        .await
        .map_err(|e| ApiError::internal(format!("Failed to clear old sources: {e}"), None))?;

    // Insert new rows
    for (var_name, param_id) in resolved_params {
        source::ActiveModel {
            id: Set(Uuid::new_v4()),
            derived_definition_id: Set(definition_id),
            parameter_id: Set(Some(*param_id)),
            variable_name: Set(var_name.clone()),
            ..Default::default()
        }
        .insert(db)
        .await
        .map_err(|e| {
            ApiError::internal(format!("Failed to insert source '{var_name}': {e}"), None)
        })?;
    }

    for (var_name, property) in &resolved.site_properties {
        source::ActiveModel {
            id: Set(Uuid::new_v4()),
            derived_definition_id: Set(definition_id),
            site_property: Set(Some(property.clone())),
            variable_name: Set(var_name.clone()),
            ..Default::default()
        }
        .insert(db)
        .await
        .map_err(|e| {
            ApiError::internal(
                format!("Failed to insert site source '{var_name}': {e}"),
                None,
            )
        })?;
    }

    Ok(())
}

/// Every source row a definition owns, cleared.
async fn delete_sources<C: ConnectionTrait>(
    db: &C,
    definition_id: Uuid,
) -> Result<(), sea_orm::DbErr> {
    source::Entity::delete_many()
        .filter(source::Column::DerivedDefinitionId.eq(definition_id))
        .exec(db)
        .await?;
    Ok(())
}

/// Why a published output parameter's code cannot be renamed, if it cannot. The code is the CSV
/// column header and the public API's identifier, so once values are stored under it or a project
/// exposes it, renaming it breaks what somebody is already reading (Q183).
fn rename_refusal(readings: u64, exposed_by: Option<&str>) -> Option<String> {
    if readings > 0 {
        return Some(format!("{readings} readings are stored under it"));
    }
    exposed_by.map(|project| format!("the project '{project}' publishes it"))
}

/// The number of readings stored under a catalog parameter.
async fn readings_under<C: ConnectionTrait>(db: &C, parameter_id: Uuid) -> Result<u64, DbErr> {
    readings::Entity::find()
        .filter(readings::Column::ParameterId.eq(parameter_id))
        .count(db)
        .await
}

/// The name of a project publishing this parameter at one of its sites, if one does.
async fn exposing_project<C: ConnectionTrait>(
    db: &C,
    parameter_id: Uuid,
) -> Result<Option<String>, DbErr> {
    let Some(slot) = site_parameters::Entity::find()
        .filter(site_parameters::Column::ParameterId.eq(parameter_id))
        .filter(site_parameters::Column::IsPublic.eq(true))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    let Some(site) = sites::Entity::find_by_id(slot.site_id).one(db).await? else {
        return Ok(None);
    };
    let Some(project_id) = site.project_id else {
        return Ok(None);
    };
    Ok(projects::Entity::find_by_id(project_id)
        .one(db)
        .await?
        .map(|project| project.name))
}

/// The catalog parameter a code already belongs to, other than this one.
async fn code_held_by<C: ConnectionTrait>(
    db: &C,
    code: &str,
    besides: Uuid,
) -> Result<Option<parameters::Model>, DbErr> {
    parameters::Entity::find()
        .filter(code_matches(code))
        .filter(parameters::Column::Id.ne(besides))
        .one(db)
        .await
}

/// The catalog row a definition's output owns, kept in step with the definition.
///
/// A code change is carried through to the catalog, because a calculation renamed on the page and
/// a catalog row left under the old code are two names for one thing and nothing reports the
/// divergence. It is refused once the code is published (Q183).
async fn update_output_parameter<C: ConnectionTrait>(
    db: &C,
    parameter_id: Uuid,
    entity: &CalculationFormula,
) -> Result<(), ApiError> {
    let mut update = parameters::Entity::update_many()
        .col_expr(
            parameters::Column::Name,
            sea_orm::sea_query::Expr::value(entity.name.clone()),
        )
        .col_expr(
            parameters::Column::DefaultUnits,
            sea_orm::sea_query::Expr::value(entity.units.clone()),
        )
        .col_expr(
            parameters::Column::Description,
            sea_orm::sea_query::Expr::value(entity.description.clone().unwrap_or_default()),
        );
    let stored = parameters::Entity::find_by_id(parameter_id)
        .one(db)
        .await
        .map_err(|e| ApiError::internal(format!("Failed to read output parameter: {e}"), None))?;
    if let Some(stored) = stored
        && stored.code != entity.code
    {
        if let Some(reason) = published_reason(db, parameter_id).await? {
            return Err(ApiError::bad_request(format!(
                "the output parameter '{}' cannot be renamed to '{}': {reason}",
                stored.code, entity.code
            )));
        }
        if let Some(other) = code_held_by(db, &entity.code, parameter_id)
            .await
            .map_err(|e| ApiError::internal(format!("Failed to look up the code: {e}"), None))?
        {
            return Err(ApiError::conflict(format!(
                "the code '{}' already belongs to the parameter '{}'",
                entity.code, other.name
            )));
        }
        update = update.col_expr(
            parameters::Column::Code,
            sea_orm::sea_query::Expr::value(entity.code.clone()),
        );
    }
    update
        .filter(parameters::Column::Id.eq(parameter_id))
        .exec(db)
        .await
        .map_err(|e| ApiError::internal(format!("Failed to update output parameter: {e}"), None))?;
    Ok(())
}

/// Why a catalog parameter's code is no longer free, when it is not.
async fn published_reason<C: ConnectionTrait>(
    db: &C,
    parameter_id: Uuid,
) -> Result<Option<String>, ApiError> {
    let readings = readings_under(db, parameter_id)
        .await
        .map_err(|e| ApiError::internal(format!("Failed to count readings: {e}"), None))?;
    let exposed = exposing_project(db, parameter_id)
        .await
        .map_err(|e| ApiError::internal(format!("Failed to read public slots: {e}"), None))?;
    Ok(rename_refusal(readings, exposed.as_deref()))
}

/// Whether a calculation publishes this catalog parameter.
async fn minted_by_calculation<C: ConnectionTrait>(
    db: &C,
    parameter_id: Uuid,
) -> Result<bool, ApiError> {
    super::models::definition::Entity::find()
        .filter(super::models::definition::Column::OutputParameterId.eq(parameter_id))
        .one(db)
        .await
        .map(|found| found.is_some())
        .map_err(|e| {
            ApiError::internal(
                format!("Failed to look up the parameter's calculation: {e}"),
                None,
            )
        })
}

/// Ensure a row in the `parameters` table exists for a derived definition's output,
/// and link it via `output_parameter_id`. Returns the parameter UUID.
async fn ensure_output_parameter<C: ConnectionTrait>(
    db: &C,
    entity: &mut CalculationFormula,
) -> Result<Option<Uuid>, ApiError> {
    // An intermediate is a step, not a measurement: nothing stores its value, so no parameter is
    // minted for it and a formula turned intermediate gives up the link it had. The row it gave up
    // is recorded, so ticking the step back recovers that parameter rather than a second one.
    if entity.intermediate {
        if let Some(given_up) = entity.output_parameter_id.take() {
            set_output_link(db, entity.id, None, Some(given_up)).await?;
            entity.given_up_parameter_id = Some(given_up);
        }
        return Ok(None);
    }
    // Reuse existing link if present
    if let Some(existing_id) = entity.output_parameter_id {
        // Keep the parameter row in sync
        update_output_parameter(db, existing_id, entity).await?;
        return Ok(Some(existing_id));
    }
    // A formula ticked back as an output takes back the parameter it published, by id, so a code
    // changed while it was a step does not send it to the adoption guard below.
    if let Some(given_up) = entity.given_up_parameter_id {
        update_output_parameter(db, given_up, entity).await?;
        set_output_link(db, entity.id, Some(given_up), None).await?;
        entity.output_parameter_id = Some(given_up);
        entity.given_up_parameter_id = None;
        return Ok(Some(given_up));
    }

    // Create or find the output parameter
    let existing = parameters::Entity::find()
        .filter(code_matches(&entity.code))
        .one(db)
        .await
        .map_err(|e| ApiError::internal(format!("Failed to lookup output parameter: {e}"), None))?;

    let param_id = if let Some(parameter) = existing {
        // A calculation mints its own output. A code already in the catalog belongs to something
        // somebody else declared, and a first save that overwrote its name and units would take it
        // over silently (Q183, Q191).
        if !minted_by_calculation(db, parameter.id).await? {
            return Err(ApiError::conflict(format!(
                "the code '{}' already belongs to the catalog parameter '{}', which no \
                 calculation produces; choose another code",
                entity.code, parameter.name
            )));
        }
        update_output_parameter(db, parameter.id, entity).await?;
        parameter.id
    } else {
        parameters::ActiveModel {
            id: Set(Uuid::new_v4()),
            code: Set(entity.code.clone()),
            name: Set(entity.name.clone()),
            default_units: Set(entity.units.clone()),
            category: Set("measurement".to_string()),
            description: Set(Some(entity.description.clone().unwrap_or_default())),
            ..Default::default()
        }
        .insert(db)
        .await
        .map_err(|e| ApiError::internal(format!("Failed to insert output parameter: {e}"), None))?
        .id
    };

    // Store the link on the definition
    set_output_link(db, entity.id, Some(param_id), None).await?;

    entity.output_parameter_id = Some(param_id);
    entity.given_up_parameter_id = None;
    Ok(Some(param_id))
}

/// Write both halves of a formula's claim on a catalog parameter: the one it publishes now, and
/// the one it published and gave up. They are written together so the pair is never half true.
async fn set_output_link<C: ConnectionTrait>(
    db: &C,
    definition_id: Uuid,
    published: Option<Uuid>,
    given_up: Option<Uuid>,
) -> Result<(), ApiError> {
    super::models::definition::Entity::update_many()
        .col_expr(
            super::models::definition::Column::OutputParameterId,
            sea_orm::sea_query::Expr::value(published),
        )
        .col_expr(
            super::models::definition::Column::GivenUpParameterId,
            sea_orm::sea_query::Expr::value(given_up),
        )
        .filter(super::models::definition::Column::Id.eq(definition_id))
        .exec(db)
        .await
        .map_err(|e| ApiError::internal(format!("Failed to link output parameter: {e}"), None))?;
    Ok(())
}

pub struct CalculationFormulaOperations;

impl CRUDOperations for CalculationFormulaOperations {
    type Resource = CalculationFormula;

    /// The change-audit trigger reads the writer from the transaction, so the label is declared on
    /// every write this entity makes, before any hook or statement on it.
    async fn after_begin<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
    ) -> Result<(), ApiError> {
        crate::common::actor::declare(db)
            .await
            .map_err(ApiError::database)
    }

    async fn after_get_one<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut CalculationFormula,
    ) -> Result<(), ApiError> {
        if let Some(parameter_id) = entity.output_parameter_id {
            entity.code_locked = published_reason(db, parameter_id).await?;
        }
        Ok(())
    }

    async fn after_get_all<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entities: &mut Vec<<CalculationFormula as CRUDResource>::ListModel>,
    ) -> Result<(), ApiError> {
        for entity in entities.iter_mut() {
            if let Some(parameter_id) = entity.output_parameter_id {
                entity.code_locked = published_reason(db, parameter_id).await?;
            }
        }
        Ok(())
    }

    /// A slot naming this definition is left as it is: `entry_mode` is the site's own declaration
    /// that it computes the parameter, and it outlives whichever calculation produced it.
    async fn before_delete<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
    ) -> Result<(), ApiError> {
        delete_sources(db, id)
            .await
            .map_err(|e| ApiError::internal(format!("Failed to delete sources: {e}"), None))?;

        Ok(())
    }

    async fn before_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        data: &<CalculationFormula as CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        validate_formula(&data.formula)?;
        let context = FormulaContext {
            tool_script_id: data.tool_script_id,
            code: &data.code,
            curve_slot: data.curve_slot.as_deref(),
        };
        let resolved = resolve_variables(db, &data.formula, &context).await?;
        // A definition being created may already have its output parameter in the catalog, and
        // anything reading that parameter is a chain this formula would close.
        let output = existing_parameter_id(db, &data.code).await?;
        validate_against_stored_graph(db, output, &resolved.parameters).await?;
        Ok(())
    }

    async fn after_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut CalculationFormula,
    ) -> Result<(), ApiError> {
        let context = FormulaContext {
            tool_script_id: entity.tool_script_id,
            code: &entity.code,
            curve_slot: entity.curve_slot.as_deref(),
        };
        let resolved = resolve_variables(db, &entity.formula, &context).await?;
        sync_sources(db, entity.id, &resolved).await?;

        // Auto-create a corresponding entry in the parameters table so this
        // derived output can be referenced as a parameter_id in site_parameters. An intermediate
        // is a step of the calculation and measures nothing, so it mints none (M180).
        ensure_output_parameter(db, entity).await?;

        if is_standalone(db, entity.id).await? {
            refuse_reducers(&entity.formula)?;
            mint_derived_version(db, entity.id, &entity.formula, None).await?;
        }

        // Populate the sources field on the response
        entity.sources = resolved
            .parameters
            .into_iter()
            .map(|(var_name, param_id)| {
                crate::routes::private::derived_parameters::models::source::DerivedParameterSource {
                    id: Uuid::nil(), // Will be fetched by CrudCrate on next read
                    derived_definition_id: entity.id,
                    parameter_id: Some(param_id),
                    site_property: None,
                    variable_name: var_name,
                    created_at: None,
                }
            })
            .collect();

        Ok(())
    }

    async fn before_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
        data: &<CalculationFormula as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        if let Some(Some(ref formula)) = data.formula {
            validate_formula(formula)?;
            // The stored row says what this definition produces, so the cycle and depth guards run
            // before the write rather than after it: a refused update must leave nothing behind.
            // It also carries the curve slot when this update does not change it.
            let stored = stored_definition(db, id).await?;
            let curve_slot = match &data.curve_slot {
                Some(slot) => slot.clone(),
                None => stored.curve_slot.clone(),
            };
            let context = FormulaContext {
                tool_script_id: stored.tool_script_id,
                code: &stored.code,
                curve_slot: curve_slot.as_deref(),
            };
            let resolved = resolve_variables(db, formula, &context).await?;
            validate_against_stored_graph(db, stored.output_parameter_id, &resolved.parameters)
                .await?;
        }
        Ok(())
    }

    async fn after_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut CalculationFormula,
    ) -> Result<(), ApiError> {
        let context = FormulaContext {
            tool_script_id: entity.tool_script_id,
            code: &entity.code,
            curve_slot: entity.curve_slot.as_deref(),
        };
        let resolved = resolve_variables(db, &entity.formula, &context).await?;
        sync_sources(db, entity.id, &resolved).await?;

        // Keep the output parameter in sync; an intermediate has none to keep.
        ensure_output_parameter(db, entity).await?;

        if is_standalone(db, entity.id).await? {
            refuse_reducers(&entity.formula)?;
            mint_derived_version(db, entity.id, &entity.formula, None).await?;
        }

        // Populate the sources field on the response
        entity.sources = resolved
            .parameters
            .into_iter()
            .map(|(var_name, param_id)| {
                crate::routes::private::derived_parameters::models::source::DerivedParameterSource {
                    id: Uuid::nil(),
                    derived_definition_id: entity.id,
                    parameter_id: Some(param_id),
                    site_property: None,
                    variable_name: var_name,
                    created_at: None,
                }
            })
            .collect();

        Ok(())
    }
}

/// What one derived run did to each slot it touched: the refusals it must report, and the slots
/// that produced a value and so close a refusal standing from an earlier run.
///
/// A divide by zero over a week of instants is one fact about the formula, not a thousand facts
/// about instants (Q172), so a run reports each slot once, carrying how many instants refused and
/// the span they cover. The hold names the first of them: `replicate_audit_holds.group_time` is
/// NOT NULL and the live-unique index for a stream-less finding is
/// `(kind, site_id, parameter_id, group_time)`, so a finding with no instant is not a thing the
/// table can hold, and the first refused instant is where the formula stopped computing.
#[derive(Default)]
pub struct DerivedPass {
    refused: HashMap<(Uuid, Uuid), Tally>,
    stored: HashSet<(Uuid, Uuid)>,
}

struct Tally {
    definition_id: Uuid,
    instants: usize,
    first: chrono::DateTime<chrono::Utc>,
    last: chrono::DateTime<chrono::Utc>,
}

impl DerivedPass {
    pub fn record(&mut self, slots: &[DerivedSlot], time: chrono::DateTime<chrono::Utc>) {
        for slot in slots {
            let key = (slot.site_id, slot.parameter_id);
            match slot.pass {
                SlotPass::Stored => {
                    self.stored.insert(key);
                }
                SlotPass::Refused => {
                    self.refused
                        .entry(key)
                        .and_modify(|tally| {
                            tally.instants += 1;
                            tally.first = Ord::min(tally.first, time);
                            tally.last = Ord::max(tally.last, time);
                        })
                        .or_insert(Tally {
                            definition_id: slot.definition_id,
                            instants: 1,
                            first: time,
                            last: time,
                        });
                }
            }
        }
    }

    /// The slots whose refusal is over: they produced a value in this run and refused nowhere in
    /// it. A slot that did both still has instants a person has not seen, so its finding stands.
    #[must_use]
    pub fn resolved(&self) -> Vec<(Uuid, Uuid)> {
        let mut slots: Vec<(Uuid, Uuid)> = self
            .stored
            .iter()
            .filter(|key| !self.refused.contains_key(key))
            .copied()
            .collect();
        slots.sort();
        slots
    }

    /// One `skipped_output` finding per slot, as the hold each is written as.
    #[must_use]
    pub fn holds(&self) -> Vec<Hold<'static>> {
        let mut holds: Vec<Hold<'static>> = self
            .refused
            .iter()
            .map(|((site_id, parameter_id), tally)| Hold {
                key: HoldKey::Slot {
                    site_id: *site_id,
                    parameter_id: *parameter_id,
                    group_time: tally.first,
                },
                kind: HoldKind::SkippedOutput,
                expected: serde_json::json!({
                    "reason": "the formula computed a value that is not finite",
                    "derived_definition_id": tally.definition_id,
                }),
                computed: serde_json::json!({
                    "instants": tally.instants,
                    "from": tally.first,
                    "to": tally.last,
                }),
                delta: serde_json::json!({}),
                status: HoldStatus::Pending,
                tool: None,
            })
            .collect();
        holds.sort_by_key(|hold| match hold.key {
            HoldKey::Slot {
                site_id,
                parameter_id,
                ..
            } => (site_id, parameter_id),
            _ => (Uuid::nil(), Uuid::nil()),
        });
        holds
    }

    /// Raise one finding per refused slot and close the findings the run repaired. Returns how
    /// many findings were raised.
    pub async fn report(&self, db: &sea_orm::DatabaseConnection) -> Result<usize, sea_orm::DbErr> {
        let holds = self.holds();
        for hold in &holds {
            upsert_hold(db, hold)
                .await
                .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?;
        }
        for (site_id, parameter_id) in self.resolved() {
            hold_model::Entity::update_many()
                .col_expr(
                    hold_model::Column::Status,
                    sea_orm::sea_query::Expr::value(HoldStatus::Superseded.as_str()),
                )
                // A null tool is what says the finding is this engine's: a calculation's skip at
                // a visit names the calculation, and is the chain's to close.
                .filter(
                    hold_model::of_kinds(
                        hold_model::in_status(
                            hold_model::slot_at_any_instant(site_id, parameter_id),
                            HoldStatus::Pending,
                        ),
                        &[HoldKind::SkippedOutput],
                    )
                    .add(hold_model::Column::Tool.is_null()),
                )
                .exec(db)
                .await?;
        }
        Ok(holds.len())
    }
}

/// What a formula leaves behind when it stops publishing a parameter: the readings stored under
/// it, the formulas that still read it, and the sites that hold a slot of it. Nothing is deleted,
/// so this is what a person confirming the tick needs to see.
pub async fn given_up_report<C: ConnectionTrait>(
    db: &C,
    code: String,
    parameter_id: Uuid,
) -> Result<crate::routes::private::tools::models::GivenUpOutput, ApiError> {
    let readings_retained = readings_under(db, parameter_id)
        .await
        .map_err(|e| ApiError::internal(format!("Failed to count readings: {e}"), None))?;
    let reader_ids: Vec<Uuid> = source::Entity::find()
        .filter(source::Column::ParameterId.eq(parameter_id))
        .all(db)
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?
        .into_iter()
        .map(|s| s.derived_definition_id)
        .collect();
    let mut read_by: Vec<String> = if reader_ids.is_empty() {
        Vec::new()
    } else {
        super::models::definition::Entity::find()
            .filter(super::models::definition::Column::Id.is_in(reader_ids))
            .order_by_asc(super::models::definition::Column::Code)
            .all(db)
            .await
            .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?
            .into_iter()
            .map(|f| f.code)
            .collect()
    };
    read_by.retain(|reader| *reader != code);
    let slot_sites: Vec<Uuid> = site_parameters::Entity::find()
        .filter(site_parameters::Column::ParameterId.eq(parameter_id))
        .all(db)
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?
        .into_iter()
        .map(|slot| slot.site_id)
        .collect();
    let sites: Vec<String> = if slot_sites.is_empty() {
        Vec::new()
    } else {
        sites::Entity::find()
            .filter(sites::Column::Id.is_in(slot_sites))
            .order_by_asc(sites::Column::Name)
            .all(db)
            .await
            .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?
            .into_iter()
            .map(|site| site.name)
            .collect()
    };
    Ok(crate::routes::private::tools::models::GivenUpOutput {
        code,
        parameter_id,
        readings_retained: i64::try_from(readings_retained).unwrap_or(i64::MAX),
        read_by,
        sites,
    })
}

#[cfg(test)]
#[path = "tests/service_tests.rs"]
mod tests;
