use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use super::model::SensorCalibration;
use super::services::{recompute_valid_until, spawn_reprocessing_job};
use crate::common::global_event_sender;

pub struct SensorCalibrationOperations;

#[async_trait]
impl CRUDOperations for SensorCalibrationOperations {
    type Resource = SensorCalibration;

    async fn before_create(
        &self,
        _db: &DatabaseConnection,
        data: &<SensorCalibration as CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        if data.slope == 0.0 {
            return Err(ApiError::bad_request(
                "Slope cannot be zero: all readings would produce a constant value".to_string(),
            ));
        }
        Ok(())
    }

    async fn before_update(
        &self,
        _db: &DatabaseConnection,
        _id: Uuid,
        data: &<SensorCalibration as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        if data.slope == Some(Some(0.0)) {
            return Err(ApiError::bad_request(
                "Slope cannot be zero: all readings would produce a constant value".to_string(),
            ));
        }
        Ok(())
    }

    async fn after_create(
        &self,
        db: &DatabaseConnection,
        entity: &mut SensorCalibration,
    ) -> Result<(), ApiError> {
        recompute_valid_until(db, entity.sensor_id)
            .await
            .map_err(ApiError::database)?;

        if let Some(events) = global_event_sender() {
            spawn_reprocessing_job(db, entity.sensor_id, "calibration_create", Some(entity.id), events)
                .await
                .map_err(ApiError::database)?;
        }

        Ok(())
    }

    async fn after_update(
        &self,
        db: &DatabaseConnection,
        entity: &mut SensorCalibration,
    ) -> Result<(), ApiError> {
        recompute_valid_until(db, entity.sensor_id)
            .await
            .map_err(ApiError::database)?;

        if let Some(events) = global_event_sender() {
            spawn_reprocessing_job(db, entity.sensor_id, "calibration_update", Some(entity.id), events)
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
                "SELECT sensor_id FROM sensor_calibrations WHERE id = $1",
                [id.into()],
            ))
            .await
            .map_err(ApiError::database)?;

        let Some(row) = row else {
            return Err(ApiError::not_found(
                "sensor_calibration",
                Some(id.to_string()),
            ));
        };
        let sensor_id: Uuid = row
            .try_get("", "sensor_id")
            .map_err(ApiError::database)?;

        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE readings SET calibration_id = NULL WHERE calibration_id = $1",
            [id.into()],
        ))
        .await
        .map_err(ApiError::database)?;

        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "DELETE FROM sensor_calibrations WHERE id = $1",
            [id.into()],
        ))
        .await
        .map_err(ApiError::database)?;

        recompute_valid_until(db, sensor_id)
            .await
            .map_err(ApiError::database)?;

        if let Some(events) = global_event_sender() {
            spawn_reprocessing_job(db, sensor_id, "calibration_delete", Some(id), events)
                .await
                .map_err(ApiError::database)?;
        }

        Ok(id)
    }
}
