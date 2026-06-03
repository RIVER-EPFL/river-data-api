use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use super::model::SensorDeployment;
use crate::common::global_event_sender;
use crate::routes::private::sensor_calibrations::services::{
    recompute_deployed_until, spawn_reprocessing_job,
};

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

        // One sensor per (site, parameter) at a time is hard-enforced by the
        // `excl_deployment_site_param_slot` constraint. Pre-check for a different sensor already in
        // this slot over an overlapping window so the operator gets a clear 400 ("recall it first")
        // instead of a raw constraint violation; the constraint remains the atomic backstop.
        let conflict = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT 1 FROM sensor_deployments d
                  WHERE d.site_id = $1
                    AND d.parameter_id = (SELECT parameter_id FROM sensors WHERE id = $2)
                    AND d.sensor_id <> $2
                    AND tstzrange(d.deployed_from, COALESCE(d.deployed_until, 'infinity'::timestamptz), '[)')
                        && tstzrange($3, COALESCE($4, 'infinity'::timestamptz), '[)')
                  LIMIT 1",
                [
                    data.site_id.into(),
                    data.sensor_id.into(),
                    data.deployed_from.into(),
                    data.deployed_until.into(),
                ],
            ))
            .await
            .map_err(ApiError::database)?;

        if conflict.is_some() {
            return Err(ApiError::bad_request(
                "Another sensor is already deployed to this site for this parameter over an \
                 overlapping period. Recall it first, then deploy."
                    .to_string(),
            ));
        }

        Ok(())
    }

    async fn after_create(
        &self,
        db: &DatabaseConnection,
        entity: &mut SensorDeployment,
    ) -> Result<(), ApiError> {
        recompute_deployed_until(db, entity.sensor_id)
            .await
            .map_err(ApiError::database)?;

        if let Some(events) = global_event_sender() {
            spawn_reprocessing_job(db, entity.sensor_id, "deployment_create", Some(entity.id), events)
                .await
                .map_err(ApiError::database)?;
        }

        Ok(())
    }

    async fn after_update(
        &self,
        db: &DatabaseConnection,
        entity: &mut SensorDeployment,
    ) -> Result<(), ApiError> {
        recompute_deployed_until(db, entity.sensor_id)
            .await
            .map_err(ApiError::database)?;

        if let Some(events) = global_event_sender() {
            spawn_reprocessing_job(db, entity.sensor_id, "deployment_update", Some(entity.id), events)
                .await
                .map_err(ApiError::database)?;
        }

        Ok(())
    }

    async fn perform_delete(
        &self,
        db: &DatabaseConnection,
        id: Uuid,
    ) -> Result<Uuid, ApiError> {
        let row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT sensor_id FROM sensor_deployments WHERE id = $1",
                [id.into()],
            ))
            .await
            .map_err(ApiError::database)?;

        let Some(row) = row else {
            return Err(ApiError::not_found(
                "sensor_deployment",
                Some(id.to_string()),
            ));
        };
        let sensor_id: Uuid = row
            .try_get("", "sensor_id")
            .map_err(ApiError::database)?;

        // readings.deployment_id has no ON DELETE action — clear references first, then delete.
        // The reprocess below re-derives deployment_id/site_id for these readings by window.
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE readings SET deployment_id = NULL WHERE deployment_id = $1",
            [id.into()],
        ))
        .await
        .map_err(ApiError::database)?;

        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "DELETE FROM sensor_deployments WHERE id = $1",
            [id.into()],
        ))
        .await
        .map_err(ApiError::database)?;

        recompute_deployed_until(db, sensor_id)
            .await
            .map_err(ApiError::database)?;

        if let Some(events) = global_event_sender() {
            spawn_reprocessing_job(db, sensor_id, "deployment_delete", Some(id), events)
                .await
                .map_err(ApiError::database)?;
        }

        Ok(id)
    }
}
