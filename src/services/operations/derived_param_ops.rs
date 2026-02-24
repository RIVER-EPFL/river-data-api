use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::DatabaseConnection;
use uuid::Uuid;

use crate::entity::derived_parameter_definitions::DerivedParameterDefinition;

fn validate_formula(formula: &str) -> Result<(), ApiError> {
    formula
        .parse::<meval::Expr>()
        .map_err(|e| ApiError::bad_request(format!("Invalid formula: {e}")))?;
    Ok(())
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
}
