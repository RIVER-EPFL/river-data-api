use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use std::collections::HashSet;
use uuid::Uuid;

use super::definition_model::DerivedParameterDefinition;
use crate::routes::private::tools::formula::free_identifiers;

/// Maximum allowed derived-from-derived chain depth.
const MAX_DERIVED_CHAIN_DEPTH: u32 = 3;

/// Mint a version of a standalone definition's formula, unless the newest one already holds that
/// text.
///
/// An edit is a new calculation rather than a correction of the old one (Q89), so the text a
/// stored value was made with stays recoverable. A definition attached to a calculation is
/// versioned by `tool_script_versions` instead, through `mint_stale_formula_versions`, so this
/// covers only the standalone kind the per-reading engine serves.
async fn mint_derived_version(
    db: &DatabaseConnection,
    definition_id: Uuid,
    formula: &str,
    actor: Option<&str>,
) -> Result<(), ApiError> {
    // The same hash the migration computes for the same text, so version 1 and every version
    // after it are hashed one way.
    let hash = migration::m20260910_000014_derived_definition_versions::formula_hash(formula);
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"INSERT INTO derived_parameter_definition_versions
              (definition_id, version_no, formula, content_hash, created_by)
          SELECT $1,
                 COALESCE((SELECT MAX(version_no) FROM derived_parameter_definition_versions
                            WHERE definition_id = $1), 0) + 1,
                 $2, $3, $4
           WHERE NOT EXISTS (
              SELECT 1 FROM derived_parameter_definition_versions v
               WHERE v.definition_id = $1 AND v.content_hash = $3
                 AND v.version_no = (SELECT MAX(version_no)
                                       FROM derived_parameter_definition_versions
                                      WHERE definition_id = $1)
           )",
        [
            definition_id.into(),
            formula.into(),
            hash.into(),
            actor.map(str::to_string).into(),
        ],
    ))
    .await
    .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
    Ok(())
}

/// Whether this definition is the standalone kind, ie. not attached to a calculation.
async fn is_standalone(db: &DatabaseConnection, definition_id: Uuid) -> Result<bool, ApiError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT tool_script_id IS NULL AS standalone \
               FROM derived_parameter_definitions WHERE id = $1",
            [definition_id.into()],
        ))
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
    Ok(row
        .and_then(|r| r.try_get::<bool>("", "standalone").ok())
        .unwrap_or(false))
}

fn validate_formula(formula: &str) -> Result<(), ApiError> {
    formula
        .parse::<meval::Expr>()
        .map_err(|e| ApiError::bad_request(format!("Invalid formula: {e}")))?;
    Ok(())
}

/// Resolve each formula variable to a parameter UUID, with strict validation.
/// Returns Vec<(`variable_name`, `parameter_id`)>.
///
/// An identifier naming a row of `constants` is not a variable at all: it resolves to the same
/// value at every site and instant, so it is left out of the sources and bound at evaluation from
/// the constants table, exactly as the script engine binds a declared constant.
async fn resolve_variables(
    db: &DatabaseConnection,
    formula: &str,
) -> Result<Vec<(String, Uuid)>, ApiError> {
    let var_names = free_identifiers(formula);
    let mut resolved = Vec::with_capacity(var_names.len());

    for var_name in &var_names {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT id FROM parameters WHERE code = $1 LIMIT 1",
                [var_name.clone().into()],
            ))
            .await
            .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;

        if let Some(row) = row {
            let id: Uuid = row
                .try_get("", "id")
                .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
            resolved.push((var_name.clone(), id));
        } else if !names_a_constant(db, var_name).await? {
            return Err(ApiError::bad_request(format!(
                "Formula variable '{var_name}' does not match any parameter or constant"
            )));
        }
    }

    Ok(resolved)
}

/// Whether the constants table holds this name.
async fn names_a_constant(db: &DatabaseConnection, name: &str) -> Result<bool, ApiError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT 1 FROM constants WHERE name = $1 LIMIT 1",
            [name.into()],
        ))
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
    async fn load(db: &DatabaseConnection) -> Result<Self, ApiError> {
        let mut graph = Self::default();
        let definitions = db
            .query_all_raw(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT id, output_parameter_id FROM derived_parameter_definitions \
                 WHERE output_parameter_id IS NOT NULL"
                    .to_string(),
            ))
            .await
            .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
        for row in &definitions {
            let id: Uuid = row
                .try_get("", "id")
                .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
            let output: Uuid = row
                .try_get("", "output_parameter_id")
                .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
            graph.definition_of.insert(output, id);
        }

        let sources = db
            .query_all_raw(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT derived_definition_id, parameter_id FROM derived_parameter_sources"
                    .to_string(),
            ))
            .await
            .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
        for row in &sources {
            let definition_id: Uuid = row
                .try_get("", "derived_definition_id")
                .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
            let parameter_id: Uuid = row
                .try_get("", "parameter_id")
                .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
            graph
                .sources_of
                .entry(definition_id)
                .or_default()
                .push(parameter_id);
        }
        Ok(graph)
    }

    /// How deep the derived chain under `parameter_id` runs. Zero for a parameter nothing derives.
    /// A cycle among the stored definitions is an error rather than an infinite walk.
    fn chain_depth(&self, parameter_id: Uuid, visited: &mut HashSet<Uuid>) -> Result<u32, String> {
        if !visited.insert(parameter_id) {
            return Err("Circular dependency detected in derived parameter chain".to_string());
        }
        let Some(definition_id) = self.definition_of.get(&parameter_id) else {
            return Ok(0);
        };
        let mut deepest = 0;
        for source in self.sources_of.get(definition_id).into_iter().flatten() {
            deepest = deepest.max(self.chain_depth(*source, visited)?);
        }
        Ok(1 + deepest)
    }

    /// Whether `target` is read, directly or through other definitions, by whatever derives
    /// `from`. This is what makes a two-definition cycle visible: the new formula reads a
    /// parameter whose own chain comes back to the parameter the new formula produces.
    fn reaches(&self, from: Uuid, target: Uuid) -> bool {
        let mut seen = HashSet::new();
        let mut stack = vec![from];
        while let Some(parameter_id) = stack.pop() {
            if parameter_id == target {
                return true;
            }
            if !seen.insert(parameter_id) {
                continue;
            }
            if let Some(definition_id) = self.definition_of.get(&parameter_id) {
                stack.extend(self.sources_of.get(definition_id).into_iter().flatten());
            }
        }
        false
    }
}

/// Refuse a set of formula variables that would make the definition producing `output_parameter_id`
/// part of a cycle, or push a chain past [`MAX_DERIVED_CHAIN_DEPTH`]. `output_parameter_id` is None
/// for a definition whose output parameter does not exist yet, which nothing can read and so cannot
/// close a cycle.
fn validate_dependency_chain(
    graph: &DerivedGraph,
    output_parameter_id: Option<Uuid>,
    resolved_params: &[(String, Uuid)],
) -> Result<(), String> {
    for (var_name, parameter_id) in resolved_params {
        if Some(*parameter_id) == output_parameter_id {
            return Err(
                "Circular dependency detected: formula references its own output parameter"
                    .to_string(),
            );
        }
        if let Some(output) = output_parameter_id
            && graph.reaches(*parameter_id, output)
        {
            return Err(format!(
                "Circular dependency detected: variable '{var_name}' is derived from this \
                 definition's own output parameter"
            ));
        }

        let mut visited = HashSet::new();
        let depth = graph.chain_depth(*parameter_id, &mut visited)?;
        if depth >= MAX_DERIVED_CHAIN_DEPTH {
            return Err(format!(
                "Derived formula chain depth exceeds maximum of {MAX_DERIVED_CHAIN_DEPTH} levels (variable '{var_name}' has depth {depth})"
            ));
        }
    }
    Ok(())
}

/// Load the graph and validate against it, the shape the CRUD hooks use.
async fn validate_against_stored_graph(
    db: &DatabaseConnection,
    output_parameter_id: Option<Uuid>,
    resolved_params: &[(String, Uuid)],
) -> Result<(), ApiError> {
    let graph = DerivedGraph::load(db).await?;
    validate_dependency_chain(&graph, output_parameter_id, resolved_params)
        .map_err(ApiError::bad_request)
}

/// The catalog parameter a code already names, if any.
async fn existing_parameter_id(
    db: &DatabaseConnection,
    code: &str,
) -> Result<Option<Uuid>, ApiError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM parameters WHERE LOWER(code) = LOWER($1) LIMIT 1",
            [code.into()],
        ))
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
    row.map(|r| {
        r.try_get::<Uuid>("", "id")
            .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))
    })
    .transpose()
}

/// The parameter a stored definition produces, and its formula.
async fn stored_definition(
    db: &DatabaseConnection,
    id: Uuid,
) -> Result<(Option<Uuid>, String), ApiError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT output_parameter_id, formula FROM derived_parameter_definitions WHERE id = $1",
            [id.into()],
        ))
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?
        .ok_or_else(|| ApiError::not_found("Derived parameter definition", None))?;
    let output = row
        .try_get::<Option<Uuid>>("", "output_parameter_id")
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
    let formula = row
        .try_get::<String>("", "formula")
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
    Ok((output, formula))
}

/// Delete existing sources and insert new ones for a derived definition.
async fn sync_sources(
    db: &DatabaseConnection,
    definition_id: Uuid,
    resolved_params: &[(String, Uuid)],
) -> Result<(), ApiError> {
    // Delete existing rows
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"DELETE FROM derived_parameter_sources WHERE derived_definition_id = $1",
        [definition_id.into()],
    ))
    .await
    .map_err(|e| ApiError::internal(format!("Failed to clear old sources: {e}"), None))?;

    // Insert new rows
    for (var_name, param_id) in resolved_params {
        db.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"INSERT INTO derived_parameter_sources (derived_definition_id, parameter_id, variable_name)
              VALUES ($1, $2, $3)",
            [definition_id.into(), (*param_id).into(), var_name.clone().into()],
        ))
        .await
        .map_err(|e| {
            ApiError::internal(format!("Failed to insert source '{var_name}': {e}"), None)
        })?;
    }

    Ok(())
}

/// Ensure a row in the `parameters` table exists for a derived definition's output,
/// and link it via `output_parameter_id`. Returns the parameter UUID.
async fn ensure_output_parameter(
    db: &DatabaseConnection,
    entity: &mut DerivedParameterDefinition,
) -> Result<Uuid, ApiError> {
    // Reuse existing link if present
    if let Some(existing_id) = entity.output_parameter_id {
        // Keep the parameter row in sync
        db.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE parameters SET name = $2, default_units = $3, description = $4
              WHERE id = $1",
            [
                existing_id.into(),
                entity.name.clone().into(),
                entity.units.clone().into(),
                entity.description.clone().unwrap_or_default().into(),
            ],
        ))
        .await
        .map_err(|e| ApiError::internal(format!("Failed to update output parameter: {e}"), None))?;
        return Ok(existing_id);
    }

    // Create or find the output parameter
    let existing = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id FROM parameters WHERE LOWER(code) = LOWER($1) LIMIT 1",
            [entity.code.clone().into()],
        ))
        .await
        .map_err(|e| ApiError::internal(format!("Failed to lookup output parameter: {e}"), None))?;

    let param_id = if let Some(row) = existing {
        let id: Uuid = row
            .try_get("", "id")
            .map_err(|e| ApiError::internal(format!("Failed to read parameter id: {e}"), None))?;
        db.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE parameters SET name = $2, default_units = $3, description = $4
              WHERE id = $1",
            [
                id.into(),
                entity.name.clone().into(),
                entity.units.clone().into(),
                entity.description.clone().unwrap_or_default().into(),
            ],
        ))
        .await
        .map_err(|e| ApiError::internal(format!("Failed to update output parameter: {e}"), None))?;
        id
    } else {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"INSERT INTO parameters (id, code, name, default_units, category, description)
                  VALUES (gen_random_uuid(), $1, $2, $3, 'measurement', $4)
                  RETURNING id",
                [
                    entity.code.clone().into(),
                    entity.name.clone().into(),
                    entity.units.clone().into(),
                    entity.description.clone().unwrap_or_default().into(),
                ],
            ))
            .await
            .map_err(|e| {
                ApiError::internal(format!("Failed to insert output parameter: {e}"), None)
            })?
            .ok_or_else(|| {
                ApiError::internal("No row returned from parameter insert".to_string(), None)
            })?;
        row.try_get::<Uuid>("", "id")
            .map_err(|e| ApiError::internal(format!("Failed to read parameter id: {e}"), None))?
    };

    // Store the link on the definition
    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE derived_parameter_definitions SET output_parameter_id = $1 WHERE id = $2",
        [param_id.into(), entity.id.into()],
    ))
    .await
    .map_err(|e| ApiError::internal(format!("Failed to link output parameter: {e}"), None))?;

    entity.output_parameter_id = Some(param_id);
    Ok(param_id)
}

pub struct DerivedParameterDefinitionOperations;

#[async_trait]
impl CRUDOperations for DerivedParameterDefinitionOperations {
    type Resource = DerivedParameterDefinition;

    /// A slot naming this definition is left as it is: `entry_mode` is the site's own declaration
    /// that it computes the parameter, and it outlives whichever calculation produced it.
    async fn before_delete(&self, db: &DatabaseConnection, id: Uuid) -> Result<(), ApiError> {
        db.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "DELETE FROM derived_parameter_sources WHERE derived_definition_id = $1",
            [id.into()],
        ))
        .await
        .map_err(|e| ApiError::internal(format!("Failed to delete sources: {e}"), None))?;

        Ok(())
    }

    async fn before_create(
        &self,
        db: &DatabaseConnection,
        data: &<DerivedParameterDefinition as CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        validate_formula(&data.formula)?;
        let resolved = resolve_variables(db, &data.formula).await?;
        // A definition being created may already have its output parameter in the catalog, and
        // anything reading that parameter is a chain this formula would close.
        let output = existing_parameter_id(db, &data.code).await?;
        validate_against_stored_graph(db, output, &resolved).await?;
        Ok(())
    }

    async fn after_create(
        &self,
        db: &DatabaseConnection,
        entity: &mut DerivedParameterDefinition,
    ) -> Result<(), ApiError> {
        let resolved = resolve_variables(db, &entity.formula).await?;
        sync_sources(db, entity.id, &resolved).await?;

        // Auto-create a corresponding entry in the parameters table so this
        // derived output can be referenced as a parameter_id in site_parameters.
        ensure_output_parameter(db, entity).await?;

        // A formula of a calculation is part of its version, so the calculation is re-minted.
        crate::routes::private::tools::calculation_versions::mint_stale_formula_versions(db, None)
            .await
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        if is_standalone(db, entity.id).await? {
            mint_derived_version(db, entity.id, &entity.formula, None).await?;
        }

        // Populate the sources field on the response
        entity.sources = resolved
            .into_iter()
            .map(|(var_name, param_id)| {
                crate::routes::private::parameters::derived::source_model::DerivedParameterSource {
                    id: Uuid::nil(), // Will be fetched by CrudCrate on next read
                    derived_definition_id: entity.id,
                    parameter_id: param_id,
                    variable_name: var_name,
                    created_at: None,
                }
            })
            .collect();

        Ok(())
    }

    async fn before_update(
        &self,
        db: &DatabaseConnection,
        id: Uuid,
        data: &<DerivedParameterDefinition as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        if let Some(Some(ref formula)) = data.formula {
            validate_formula(formula)?;
            let resolved = resolve_variables(db, formula).await?;
            // The stored row says what this definition produces, so the cycle and depth guards run
            // before the write rather than after it: a refused update must leave nothing behind.
            let (output, _) = stored_definition(db, id).await?;
            validate_against_stored_graph(db, output, &resolved).await?;
        }
        Ok(())
    }

    async fn after_update(
        &self,
        db: &DatabaseConnection,
        entity: &mut DerivedParameterDefinition,
    ) -> Result<(), ApiError> {
        let resolved = resolve_variables(db, &entity.formula).await?;
        sync_sources(db, entity.id, &resolved).await?;

        // Keep the output parameter in sync
        ensure_output_parameter(db, entity).await?;

        crate::routes::private::tools::calculation_versions::mint_stale_formula_versions(db, None)
            .await
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        if is_standalone(db, entity.id).await? {
            mint_derived_version(db, entity.id, &entity.formula, None).await?;
        }

        // Populate the sources field on the response
        entity.sources = resolved
            .into_iter()
            .map(|(var_name, param_id)| {
                crate::routes::private::parameters::derived::source_model::DerivedParameterSource {
                    id: Uuid::nil(),
                    derived_definition_id: entity.id,
                    parameter_id: param_id,
                    variable_name: var_name,
                    created_at: None,
                }
            })
            .collect();

        Ok(())
    }

    async fn after_delete(&self, db: &DatabaseConnection, _id: Uuid) -> Result<(), ApiError> {
        crate::routes::private::tools::calculation_versions::mint_stale_formula_versions(db, None)
            .await
            .map_err(|e| ApiError::bad_request(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::{DerivedGraph, MAX_DERIVED_CHAIN_DEPTH, validate_dependency_chain};
    use std::collections::HashMap;
    use uuid::Uuid;

    /// A definition producing `output`, reading `sources`. Its own code never enters the graph:
    /// what it produces is `output_parameter_id`, and the two are routinely spelled differently.
    fn graph(definitions: &[(Uuid, Uuid, Vec<Uuid>)]) -> DerivedGraph {
        let mut definition_of = HashMap::new();
        let mut sources_of = HashMap::new();
        for (id, output, sources) in definitions {
            definition_of.insert(*output, *id);
            sources_of.insert(*id, sources.clone());
        }
        DerivedGraph {
            definition_of,
            sources_of,
        }
    }

    #[test]
    fn a_definition_is_found_by_what_it_produces() {
        let (definition, output, input) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let g = graph(&[(definition, output, vec![input])]);
        assert_eq!(
            g.chain_depth(output, &mut std::collections::HashSet::new())
                .unwrap(),
            1,
            "the walk reaches a definition whose code and output parameter are spelled differently"
        );
        assert_eq!(
            g.chain_depth(input, &mut std::collections::HashSet::new())
                .unwrap(),
            0,
            "a parameter nothing derives is the bottom of the chain"
        );
    }

    #[test]
    fn a_two_definition_cycle_is_refused() {
        let (a, a_out, b, b_out) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        // A reads B's output; the formula under test produces B's output and reads A's.
        let g = graph(&[(a, a_out, vec![b_out]), (b, b_out, vec![a_out])]);
        let err =
            validate_dependency_chain(&g, Some(b_out), &[("a".to_string(), a_out)]).unwrap_err();
        assert!(err.contains("Circular dependency"), "{err}");
    }

    #[test]
    fn a_formula_reading_its_own_output_is_refused() {
        let output = Uuid::new_v4();
        let err =
            validate_dependency_chain(&graph(&[]), Some(output), &[("self".to_string(), output)])
                .unwrap_err();
        assert!(err.contains("its own output parameter"), "{err}");
    }

    #[test]
    fn a_chain_at_the_depth_limit_is_refused() {
        let outputs: Vec<Uuid> = (0..MAX_DERIVED_CHAIN_DEPTH)
            .map(|_| Uuid::new_v4())
            .collect();
        let base = Uuid::new_v4();
        let mut definitions = Vec::new();
        let mut below = base;
        for output in &outputs {
            definitions.push((Uuid::new_v4(), *output, vec![below]));
            below = *output;
        }
        let g = graph(&definitions);
        let deepest = *outputs.last().unwrap();
        let err =
            validate_dependency_chain(&g, Some(Uuid::new_v4()), &[("x".to_string(), deepest)])
                .unwrap_err();
        assert!(err.contains("chain depth exceeds maximum"), "{err}");
    }

    #[test]
    fn a_definition_with_no_output_parameter_yet_closes_no_cycle() {
        let input = Uuid::new_v4();
        validate_dependency_chain(&graph(&[]), None, &[("x".to_string(), input)]).unwrap();
    }
}
