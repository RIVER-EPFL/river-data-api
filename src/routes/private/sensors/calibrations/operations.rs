use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use super::model::SensorCalibration;
use super::service::recompute_valid_until;

pub struct SensorCalibrationOperations;

const DUPLICATE_INSTANT: &str =
    "A windowed calibration for this sensor and parameter already starts at that instant. Two \
     curves sharing a valid_from leave one with an empty window: edit the existing curve, or start \
     this one at a different instant.";

/// Whether another windowed curve on the same `(sensor, parameter)` channel already opens at
/// `valid_from`. Zero-width windows are what a duplicate produces (`recompute_valid_until` chains
/// each curve's end to the next curve's start), and a curve applying to nothing is invisible in
/// every reading but visible in the editor.
///
/// A request that names no parameter matches any curve at that instant. The
/// `inherit_calibration_parameter_id` BEFORE-INSERT trigger fills a windowed curve's parameter in
/// from the sensor's first parameter-bearing curve, so the channel the row lands on is not knowable
/// here without restating the trigger's rule: treating the instant itself as taken is the answer
/// that needs no second copy of it.
async fn duplicate_instant_exists(
    db: &DatabaseConnection,
    sensor_id: Uuid,
    parameter_id: Option<Uuid>,
    valid_from: chrono::DateTime<chrono::Utc>,
    exclude: Option<Uuid>,
) -> Result<bool, ApiError> {
    let found = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT 1 AS one FROM sensor_calibrations
              WHERE sensor_id = $1
                AND mode = 'windowed'
                AND ($2::uuid IS NULL OR parameter_id IS NOT DISTINCT FROM $2::uuid)
                AND valid_from = $3
                AND ($4::uuid IS NULL OR id <> $4::uuid)
              LIMIT 1",
            [
                sensor_id.into(),
                parameter_id.into(),
                valid_from.into(),
                exclude.into(),
            ],
        ))
        .await
        .map_err(ApiError::database)?;
    Ok(found.is_some())
}

#[async_trait]
impl CRUDOperations for SensorCalibrationOperations {
    type Resource = SensorCalibration;

    async fn before_create(
        &self,
        db: &DatabaseConnection,
        data: &<SensorCalibration as CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        if data.slope == 0.0 {
            return Err(ApiError::bad_request(
                "Slope cannot be zero: all readings would produce a constant value".to_string(),
            ));
        }

        // Only windowed curves chain. An instant (lab grab) curve may share an instant with a
        // windowed one, it is matched by id at the grab and never by window.
        if data.mode.as_deref().unwrap_or("windowed") == "windowed"
            && duplicate_instant_exists(db, data.sensor_id, data.parameter_id, data.valid_from, None)
                .await?
        {
            return Err(ApiError::bad_request(DUPLICATE_INSTANT.to_string()));
        }
        Ok(())
    }

    async fn before_update(
        &self,
        db: &DatabaseConnection,
        id: Uuid,
        data: &<SensorCalibration as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        if data.slope == Some(Some(0.0)) {
            return Err(ApiError::bad_request(
                "Slope cannot be zero: all readings would produce a constant value".to_string(),
            ));
        }

        // Moving a curve's start onto another curve's start is the same collision as creating one
        // there. `mode` is immutable after create, so the row's own mode decides whether it chains.
        let Some(Some(new_from)) = data.valid_from else {
            return Ok(());
        };
        let Some(existing) = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT sensor_id, parameter_id, mode FROM sensor_calibrations WHERE id = $1",
                [id.into()],
            ))
            .await
            .map_err(ApiError::database)?
        else {
            return Ok(()); // unknown id, let CrudCrate's update produce the 404
        };
        let mode: String = existing.try_get("", "mode").map_err(ApiError::database)?;
        if mode != "windowed" {
            return Ok(());
        }
        let sensor_id: Uuid = existing.try_get("", "sensor_id").map_err(ApiError::database)?;
        let parameter_id: Option<Uuid> = existing.try_get("", "parameter_id").ok();
        let parameter_id = match data.parameter_id {
            Some(patched) => patched,
            None => parameter_id,
        };

        if duplicate_instant_exists(db, sensor_id, parameter_id, new_from, Some(id)).await? {
            return Err(ApiError::bad_request(DUPLICATE_INSTANT.to_string()));
        }
        Ok(())
    }

    async fn after_create(
        &self,
        db: &DatabaseConnection,
        entity: &mut SensorCalibration,
    ) -> Result<(), ApiError> {
        // Instant (lab grab) curves take no part in windowed chaining and drive no reprocessing:
        // they apply only to the single grab reading that names them.
        if entity.mode != "windowed" {
            return Ok(());
        }

        recompute_valid_until(db, entity.sensor_id)
            .await
            .map_err(ApiError::database)?;

        crate::routes::private::reprocessing_jobs::worker::enqueue(
            db,
            "calibration_create",
            Some(entity.sensor_id),
            Some(entity.id),
            &serde_json::json!({ "sensor_id": entity.sensor_id }),
            None,
        )
        .await
        .map_err(ApiError::database)?;

        Ok(())
    }

    async fn after_update(
        &self,
        db: &DatabaseConnection,
        entity: &mut SensorCalibration,
    ) -> Result<(), ApiError> {
        if entity.mode != "windowed" {
            return Ok(());
        }

        recompute_valid_until(db, entity.sensor_id)
            .await
            .map_err(ApiError::database)?;

        crate::routes::private::reprocessing_jobs::worker::enqueue(
            db,
            "calibration_update",
            Some(entity.sensor_id),
            Some(entity.id),
            &serde_json::json!({ "sensor_id": entity.sensor_id }),
            None,
        )
        .await
        .map_err(ApiError::database)?;

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

        // `calibration_id` is neither the segmentby nor the time dimension, so no compressed batch
        // can be excluded by metadata: the clear goes through the guarded writer, which lifts the
        // decompression cap it would otherwise hit on a sensor with historical readings.
        crate::common::bulk_write::guarded_mutation(
            db,
            Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE readings SET calibration_id = NULL WHERE calibration_id = $1",
                [id.into()],
            ),
        )
        .await
        .map_err(|e| {
            ApiError::internal(
                "Failed to clear the calibration's readings references",
                Some(e.to_string()),
            )
        })?;

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

        crate::routes::private::reprocessing_jobs::worker::enqueue(
            db,
            "calibration_delete",
            Some(sensor_id),
            Some(id),
            &serde_json::json!({ "sensor_id": sensor_id }),
            None,
        )
        .await
        .map_err(ApiError::database)?;

        Ok(id)
    }
}
