use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::DatabaseConnection;
use uuid::Uuid;

use crate::entity::sensor_calibrations::SensorCalibration;
use crate::services::calibration::recalculate_for_calibration;

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
        let db = db.clone();
        let id = entity.id;
        tokio::spawn(async move {
            if let Err(e) = recalculate_for_calibration(&db, id).await {
                tracing::error!(error = %e, calibration_id = %id, "Calibration recalculation failed");
            }
        });
        Ok(())
    }

    async fn after_update(
        &self,
        db: &DatabaseConnection,
        entity: &mut SensorCalibration,
    ) -> Result<(), ApiError> {
        let db = db.clone();
        let id = entity.id;
        tokio::spawn(async move {
            if let Err(e) = recalculate_for_calibration(&db, id).await {
                tracing::error!(error = %e, calibration_id = %id, "Calibration recalculation failed");
            }
        });
        Ok(())
    }
}
