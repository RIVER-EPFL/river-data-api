use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use std::collections::HashSet;
use uuid::Uuid;

use crate::entity::derived_parameter_definitions::DerivedParameterDefinition;

/// Maximum allowed derived-from-derived chain depth.
const MAX_DERIVED_CHAIN_DEPTH: u32 = 3;

/// Math functions/constants recognized by meval — not variable names
const MATH_BUILTINS: &[&str] = &[
    "sqrt", "abs", "ln", "log", "exp", "sin", "cos", "tan", "asin", "acos", "atan",
    "sinh", "cosh", "tanh", "floor", "ceil", "round", "signum",
    "min", "max", "pi", "e",
];

fn validate_formula(formula: &str) -> Result<(), ApiError> {
    formula
        .parse::<meval::Expr>()
        .map_err(|e| ApiError::bad_request(format!("Invalid formula: {e}")))?;
    Ok(())
}

/// Extract variable names from a formula string (identifiers that aren't math builtins)
fn extract_variable_names(formula: &str) -> Vec<String> {
    let builtins: HashSet<&str> = MATH_BUILTINS.iter().copied().collect();
    let re_tokens: Vec<&str> = {
        // Simple tokenizer: match word-character sequences
        let mut tokens = Vec::new();
        let mut start = None;
        for (i, c) in formula.char_indices() {
            if c.is_alphanumeric() || c == '_' {
                if start.is_none() {
                    start = Some(i);
                }
            } else if let Some(s) = start {
                tokens.push(&formula[s..i]);
                start = None;
            }
        }
        if let Some(s) = start {
            tokens.push(&formula[s..]);
        }
        tokens
    };

    let mut seen = HashSet::new();
    let mut vars = Vec::new();
    for token in re_tokens {
        // Skip if it's a number (starts with digit)
        if token.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        if !builtins.contains(token) && seen.insert(token.to_string()) {
            vars.push(token.to_string());
        }
    }
    vars
}

/// Resolve each formula variable to a parameter UUID, with strict validation.
/// Returns Vec<(`variable_name`, `parameter_id`)>.
async fn resolve_variables(
    db: &DatabaseConnection,
    formula: &str,
) -> Result<Vec<(String, Uuid)>, ApiError> {
    let var_names = extract_variable_names(formula);
    let mut resolved = Vec::with_capacity(var_names.len());

    for var_name in &var_names {
        let row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT id FROM parameters WHERE name = $1 LIMIT 1",
                [var_name.clone().into()],
            ))
            .await
            .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;

        if let Some(row) = row {
            let id: Uuid = row
                .try_get("", "id")
                .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
            resolved.push((var_name.clone(), id));
        } else {
            return Err(ApiError::bad_request(format!(
                "Formula variable '{var_name}' does not match any parameter in the catalog"
            )));
        }
    }

    Ok(resolved)
}

/// Check if a parameter is the output of a derived definition.
/// Returns the `derived_definition_id` if so.
async fn find_derived_definition_for_param(
    db: &DatabaseConnection,
    parameter_id: Uuid,
) -> Result<Option<Uuid>, ApiError> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT dpd.id FROM derived_parameter_definitions dpd
              JOIN parameters p ON p.name = dpd.name
              WHERE p.id = $1",
            [parameter_id.into()],
        ))
        .await
        .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;

    Ok(row
        .and_then(|r| r.try_get::<Uuid>("", "id").ok()))
}

/// Recursively compute the depth of a derived parameter chain.
/// Returns 0 for non-derived parameters.
fn compute_chain_depth<'a>(
    db: &'a DatabaseConnection,
    parameter_id: Uuid,
    visited: &'a mut HashSet<Uuid>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u32, ApiError>> + Send + 'a>> {
    Box::pin(async move {
        if visited.contains(&parameter_id) {
            return Err(ApiError::bad_request(
                "Circular dependency detected in derived parameter chain".to_string(),
            ));
        }
        visited.insert(parameter_id);

        let def_id = match find_derived_definition_for_param(db, parameter_id).await? {
            Some(id) => id,
            None => return Ok(0), // Not a derived parameter
        };

        // Get this definition's sources
        let source_rows = db
            .query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT parameter_id FROM derived_parameter_sources WHERE derived_definition_id = $1",
                [def_id.into()],
            ))
            .await
            .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;

        let mut max_child_depth = 0u32;
        for row in &source_rows {
            let child_param_id: Uuid = row
                .try_get("", "parameter_id")
                .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
            let child_depth = compute_chain_depth(db, child_param_id, visited).await?;
            max_child_depth = max_child_depth.max(child_depth);
        }

        Ok(1 + max_child_depth)
    })
}

/// Validate that adding dependencies doesn't create cycles or exceed max depth.
async fn validate_dependency_chain(
    db: &DatabaseConnection,
    definition_name: &str,
    resolved_params: &[(String, Uuid)],
) -> Result<(), ApiError> {
    for (var_name, parameter_id) in resolved_params {
        let mut visited = HashSet::new();

        // Check if this source parameter's chain leads back to our definition
        // by checking if any ancestor has the same name as our definition
        let depth = compute_chain_depth(db, *parameter_id, &mut visited).await?;

        if depth >= MAX_DERIVED_CHAIN_DEPTH {
            return Err(ApiError::bad_request(format!(
                "Derived formula chain depth exceeds maximum of {MAX_DERIVED_CHAIN_DEPTH} levels (variable '{var_name}' has depth {depth})"
            )));
        }

        // Cycle check: if the source parameter resolves to a derived definition
        // whose chain references a parameter with the same name as this definition,
        // that would create a cycle
        if let Some(_def_id) = find_derived_definition_for_param(db, *parameter_id).await? {
            // Check if any parameter in the chain matches our definition name
            let cycle_row = db
                .query_one(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    r"SELECT 1 FROM parameters WHERE name = $1 AND id = $2",
                    [definition_name.into(), (*parameter_id).into()],
                ))
                .await
                .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;

            if cycle_row.is_some() {
                return Err(ApiError::bad_request(
                    "Circular dependency detected: formula references its own output parameter"
                        .to_string(),
                ));
            }
        }
    }

    Ok(())
}

/// Delete existing sources and insert new ones for a derived definition.
async fn sync_sources(
    db: &DatabaseConnection,
    definition_id: Uuid,
    resolved_params: &[(String, Uuid)],
) -> Result<(), ApiError> {
    // Delete existing rows
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"DELETE FROM derived_parameter_sources WHERE derived_definition_id = $1",
        [definition_id.into()],
    ))
    .await
    .map_err(|e| ApiError::internal(format!("Failed to clear old sources: {e}"), None))?;

    // Insert new rows
    for (var_name, param_id) in resolved_params {
        db.execute(Statement::from_sql_and_values(
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
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE parameters SET display_name = $2, default_units = $3, description = $4
              WHERE id = $1",
            [
                existing_id.into(),
                entity.display_name.clone().into(),
                entity.units.clone().into(),
                entity.description.clone().unwrap_or_default().into(),
            ],
        ))
        .await
        .map_err(|e| ApiError::internal(format!("Failed to update output parameter: {e}"), None))?;
        return Ok(existing_id);
    }

    // Create or find the output parameter
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"INSERT INTO parameters (id, name, display_name, default_units, category, data_type, description)
              VALUES (gen_random_uuid(), $1, $2, $3, 'derived', 'float', $4)
              ON CONFLICT (name) DO UPDATE SET
                display_name = EXCLUDED.display_name,
                default_units = EXCLUDED.default_units,
                description = EXCLUDED.description
              RETURNING id",
            [
                entity.name.clone().into(),
                entity.display_name.clone().into(),
                entity.units.clone().into(),
                entity.description.clone().unwrap_or_default().into(),
            ],
        ))
        .await
        .map_err(|e| ApiError::internal(format!("Failed to ensure output parameter: {e}"), None))?
        .ok_or_else(|| ApiError::internal("No row returned from parameter upsert".to_string(), None))?;

    let param_id: Uuid = row
        .try_get("", "id")
        .map_err(|e| ApiError::internal(format!("Failed to read parameter id: {e}"), None))?;

    // Store the link on the definition
    db.execute(Statement::from_sql_and_values(
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

    async fn before_create(
        &self,
        db: &DatabaseConnection,
        data: &<DerivedParameterDefinition as CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        validate_formula(&data.formula)?;
        let resolved = resolve_variables(db, &data.formula).await?;
        validate_dependency_chain(db, &data.name, &resolved).await?;
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

        // Populate the sources field on the response
        entity.sources = resolved
            .into_iter()
            .map(|(var_name, param_id)| {
                crate::entity::derived_parameter_sources::DerivedParameterSource {
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
        _id: Uuid,
        data: &<DerivedParameterDefinition as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        if let Some(Some(ref formula)) = data.formula {
            validate_formula(formula)?;
            // We validate variables here but need the definition name for cycle check.
            // We'll do full validation in after_update when we have the entity.
            let resolved = resolve_variables(db, formula).await?;
            // We can't easily get the name from the UpdateModel, so cycle/depth
            // validation happens in after_update
            drop(resolved);
        }
        Ok(())
    }

    async fn after_update(
        &self,
        db: &DatabaseConnection,
        entity: &mut DerivedParameterDefinition,
    ) -> Result<(), ApiError> {
        let resolved = resolve_variables(db, &entity.formula).await?;
        validate_dependency_chain(db, &entity.name, &resolved).await?;
        sync_sources(db, entity.id, &resolved).await?;

        // Keep the output parameter in sync
        ensure_output_parameter(db, entity).await?;

        // Populate the sources field on the response
        entity.sources = resolved
            .into_iter()
            .map(|(var_name, param_id)| {
                crate::entity::derived_parameter_sources::DerivedParameterSource {
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
}
