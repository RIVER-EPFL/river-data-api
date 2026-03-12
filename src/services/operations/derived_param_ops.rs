use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use std::collections::HashSet;
use uuid::Uuid;

use crate::entity::derived_parameter_definitions::DerivedParameterDefinition;

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

/// Resolve variable names to parameter type names by querying the parameters table
async fn resolve_required_param_types(
    db: &DatabaseConnection,
    formula: &str,
) -> Result<serde_json::Value, ApiError> {
    let var_names = extract_variable_names(formula);
    if var_names.is_empty() {
        return Ok(serde_json::json!([]));
    }

    // Query parameters table to find matching names
    // We resolve variable names against the global parameters (parameter_types) table
    let mut matched = Vec::new();
    for var_name in &var_names {
        let row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT name FROM parameters WHERE name = $1 LIMIT 1",
                [var_name.clone().into()],
            ))
            .await
            .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;

        if let Some(row) = row {
            let name: String = row
                .try_get("", "name")
                .map_err(|e| ApiError::internal(format!("DB error: {e}"), None))?;
            matched.push(name);
        } else {
            // Variable not found in parameter types — still include it as-is
            // (the formula may reference parameter type names that haven't been created yet)
            matched.push(var_name.clone());
        }
    }

    Ok(serde_json::json!(matched))
}

pub struct DerivedParameterDefinitionOperations;

#[async_trait]
impl CRUDOperations for DerivedParameterDefinitionOperations {
    type Resource = DerivedParameterDefinition;

    async fn before_create(
        &self,
        _db: &DatabaseConnection,
        data: &<DerivedParameterDefinition as CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        validate_formula(&data.formula)?;
        Ok(())
    }

    async fn after_create(
        &self,
        db: &DatabaseConnection,
        entity: &mut DerivedParameterDefinition,
    ) -> Result<(), ApiError> {
        let resolved = resolve_required_param_types(db, &entity.formula).await?;
        // Update the entity's required_parameter_types in the database
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE derived_parameter_definitions SET required_parameter_types = $1 WHERE id = $2",
            [resolved.clone().into(), entity.id.into()],
        ))
        .await
        .map_err(|e| ApiError::internal(format!("Failed to update required_parameter_types: {e}"), None))?;
        entity.required_parameter_types = resolved;
        Ok(())
    }

    async fn before_update(
        &self,
        _db: &DatabaseConnection,
        _id: Uuid,
        data: &<DerivedParameterDefinition as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        if let Some(Some(ref formula)) = data.formula {
            validate_formula(formula)?;
        }
        Ok(())
    }

    async fn after_update(
        &self,
        db: &DatabaseConnection,
        entity: &mut DerivedParameterDefinition,
    ) -> Result<(), ApiError> {
        let resolved = resolve_required_param_types(db, &entity.formula).await?;
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE derived_parameter_definitions SET required_parameter_types = $1 WHERE id = $2",
            [resolved.clone().into(), entity.id.into()],
        ))
        .await
        .map_err(|e| ApiError::internal(format!("Failed to update required_parameter_types: {e}"), None))?;
        entity.required_parameter_types = resolved;
        Ok(())
    }
}
