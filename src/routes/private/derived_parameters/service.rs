use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, Set,
    Statement, TransactionTrait,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use uuid::Uuid;

use super::models::definition::CalculationFormula;
use super::models::source;
use crate::routes::private::constants;
use crate::routes::private::parameters;
use crate::routes::private::tools::service::free_identifiers;

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
/// versioned by `tool_script_versions` instead, through `mint_stale_formula_versions`, so this
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
    Ok(row.is_some_and(|d| d.tool_script_id.is_none()))
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
) -> Result<ResolvedSources, ApiError> {
    resolve_identifiers(db, &free_identifiers(formula)).await
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
) -> Result<(Option<Uuid>, String), ApiError> {
    let stored = super::models::definition::Entity::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?
        .ok_or_else(|| ApiError::not_found("Derived parameter definition", None))?;
    Ok((stored.output_parameter_id, stored.formula))
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

/// The catalog row a definition's output owns, kept in step with the definition.
async fn update_output_parameter<C: ConnectionTrait>(
    db: &C,
    parameter_id: Uuid,
    entity: &CalculationFormula,
) -> Result<(), sea_orm::DbErr> {
    parameters::Entity::update_many()
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
        )
        .filter(parameters::Column::Id.eq(parameter_id))
        .exec(db)
        .await?;
    Ok(())
}

/// Ensure a row in the `parameters` table exists for a derived definition's output,
/// and link it via `output_parameter_id`. Returns the parameter UUID.
async fn ensure_output_parameter<C: ConnectionTrait>(
    db: &C,
    entity: &mut CalculationFormula,
) -> Result<Option<Uuid>, ApiError> {
    // An intermediate is a step, not a measurement: nothing stores its value, so no parameter is
    // minted for it and a formula turned intermediate gives up the link it had.
    if entity.intermediate {
        entity.output_parameter_id = None;
        return Ok(None);
    }
    // Reuse existing link if present
    if let Some(existing_id) = entity.output_parameter_id {
        // Keep the parameter row in sync
        update_output_parameter(db, existing_id, entity)
            .await
            .map_err(|e| {
                ApiError::internal(format!("Failed to update output parameter: {e}"), None)
            })?;
        return Ok(Some(existing_id));
    }

    // Create or find the output parameter
    let existing = parameters::Entity::find()
        .filter(code_matches(&entity.code))
        .one(db)
        .await
        .map_err(|e| ApiError::internal(format!("Failed to lookup output parameter: {e}"), None))?;

    let param_id = if let Some(parameter) = existing {
        update_output_parameter(db, parameter.id, entity)
            .await
            .map_err(|e| {
                ApiError::internal(format!("Failed to update output parameter: {e}"), None)
            })?;
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
    super::models::definition::Entity::update_many()
        .col_expr(
            super::models::definition::Column::OutputParameterId,
            sea_orm::sea_query::Expr::value(param_id),
        )
        .filter(super::models::definition::Column::Id.eq(entity.id))
        .exec(db)
        .await
        .map_err(|e| ApiError::internal(format!("Failed to link output parameter: {e}"), None))?;

    entity.output_parameter_id = Some(param_id);
    Ok(Some(param_id))
}

pub struct CalculationFormulaOperations;

impl CRUDOperations for CalculationFormulaOperations {
    type Resource = CalculationFormula;

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
        let resolved = resolve_variables(db, &data.formula).await?;
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
        let resolved = resolve_variables(db, &entity.formula).await?;
        sync_sources(db, entity.id, &resolved).await?;

        // Auto-create a corresponding entry in the parameters table so this
        // derived output can be referenced as a parameter_id in site_parameters. An intermediate
        // is a step of the calculation and measures nothing, so it mints none (M180).
        ensure_output_parameter(db, entity).await?;

        // A formula of a calculation is part of its version, so the calculation is re-minted.
        crate::routes::private::tools::service::mint_stale_formula_versions(db, None)
            .await
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        if is_standalone(db, entity.id).await? {
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
            let resolved = resolve_variables(db, formula).await?;
            // The stored row says what this definition produces, so the cycle and depth guards run
            // before the write rather than after it: a refused update must leave nothing behind.
            let (output, _) = stored_definition(db, id).await?;
            validate_against_stored_graph(db, output, &resolved.parameters).await?;
        }
        Ok(())
    }

    async fn after_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut CalculationFormula,
    ) -> Result<(), ApiError> {
        let resolved = resolve_variables(db, &entity.formula).await?;
        sync_sources(db, entity.id, &resolved).await?;

        // Keep the output parameter in sync; an intermediate has none to keep.
        ensure_output_parameter(db, entity).await?;

        crate::routes::private::tools::service::mint_stale_formula_versions(db, None)
            .await
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        if is_standalone(db, entity.id).await? {
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

    async fn after_delete<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        _id: Uuid,
    ) -> Result<(), ApiError> {
        crate::routes::private::tools::service::mint_stale_formula_versions(db, None)
            .await
            .map_err(|e| ApiError::bad_request(e.to_string()))
    }
}

#[cfg(test)]
#[path = "tests/service_tests.rs"]
mod tests;
