use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations};
use sea_orm::DatabaseConnection;

use crate::entity::sensor_calibrations::SensorCalibration;
use crate::services::calibration::recalculate_for_calibration;

pub struct SensorCalibrationOperations;

#[async_trait]
impl CRUDOperations for SensorCalibrationOperations {
    type Resource = SensorCalibration;

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
