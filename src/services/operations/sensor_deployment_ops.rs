use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

use crate::entity::sensor_deployments::SensorDeployment;

pub struct SensorDeploymentOperations;

#[async_trait]
impl CRUDOperations for SensorDeploymentOperations {
    type Resource = SensorDeployment;

    async fn before_create(
        &self,
        db: &DatabaseConnection,
        data: &<SensorDeployment as CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        let result = db
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"UPDATE sensor_deployments
                  SET deployed_until = $1
                  WHERE sensor_id = $2 AND deployed_until IS NULL",
                [data.deployed_from.into(), data.sensor_id.into()],
            ))
            .await
            .map_err(ApiError::database)?;

        if result.rows_affected() > 0 {
            tracing::info!(
                sensor_id = %data.sensor_id,
                recalled = result.rows_affected(),
                "Auto-recalled active deployment(s) on new deploy"
            );
        }

        Ok(())
    }
}
