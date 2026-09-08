use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource, MergeIntoActiveModel};
use sea_orm::{ConnectionTrait, DatabaseConnection, EntityTrait, Statement};
use uuid::Uuid;

use super::model::SensorCalibration;
use super::service::recompute_valid_until;

pub struct SensorCalibrationOperations;

const DUPLICATE_INSTANT: &str = "A calibration for this sensor and parameter already starts at that instant. Two \
     curves sharing a valid_from leave one with an empty window: edit the existing curve, or start \
     this one at a different instant.";

/// Whether another curve on the same `(sensor, parameter)` channel already opens at
/// `valid_from`. Zero-width windows are what a duplicate produces (`recompute_valid_until` chains
/// each curve's end to the next curve's start), and a curve applying to nothing is invisible in
/// every reading but visible in the editor.
///
/// A request that names no parameter matches any curve at that instant. The
/// `inherit_calibration_parameter_id` BEFORE-INSERT trigger fills a curve's parameter in
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
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT 1 AS one FROM sensor_calibrations
              WHERE sensor_id = $1
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

/// Whether an update's `valid_until` makes the row's end date an operator's or the chain's again.
/// `None` when the update carried no end date at all, which leaves the provenance as it stands.
fn valid_until_provenance(
    valid_until: Option<Option<chrono::DateTime<chrono::Utc>>>,
) -> Option<bool> {
    match valid_until {
        Some(Some(_)) => Some(true),
        // Cleared, so the window chain reclaims the row on the next recompute.
        Some(None) => Some(false),
        None => None,
    }
}

/// Chain the sensor's windows, then enqueue the reprocess that re-derives its readings. Every
/// calibration write does exactly this and differs only in the trigger it records, so the three
/// hooks call it rather than restating it.
async fn reprocess_after_calibration_write(
    db: &DatabaseConnection,
    trigger: &str,
    sensor_id: Uuid,
    calibration_id: Uuid,
) -> Result<(), ApiError> {
    recompute_valid_until(db, sensor_id)
        .await
        .map_err(ApiError::database)?;

    crate::routes::private::reprocessing_jobs::worker::enqueue(
        db,
        trigger,
        Some(sensor_id),
        Some(calibration_id),
        &serde_json::json!({ "sensor_id": sensor_id }),
        None,
    )
    .await
    .map_err(ApiError::database)?;

    Ok(())
}

/// How many readings hold a live `calibration_pin` naming this curve, or `None` when there are
/// none. The repoint that clears the way for the delete excludes pinned rows, so each one still
/// names the curve when the DELETE runs and the foreign key refuses the statement.
async fn pinned_readings(db: &DatabaseConnection, id: Uuid) -> Result<Option<i64>, ApiError> {
    let sql = format!(
        "SELECT count(*) AS n FROM readings r
         WHERE r.calibration_id = $1 AND NOT ({not_pinned})",
        not_pinned = crate::routes::private::readings::decisions::not_pinned_sql(
            "r",
            crate::routes::private::readings::decisions::Kind::CalibrationPin
        ),
    );
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            [id.into()],
        ))
        .await
        .map_err(ApiError::database)?;
    let n = row
        .map(|r| r.try_get::<i64>("", "n"))
        .transpose()
        .map_err(ApiError::database)?
        .unwrap_or_default();
    Ok((n > 0).then_some(n))
}

#[async_trait]
impl CRUDOperations for SensorCalibrationOperations {
    type Resource = SensorCalibration;

    async fn before_create(
        &self,
        db: &DatabaseConnection,
        data: &<SensorCalibration as CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        if duplicate_instant_exists(db, data.sensor_id, data.parameter_id, data.valid_from, None)
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
        if data.valid_from.is_none() && data.valid_until.is_none() {
            return Ok(());
        }
        let Some(existing) = super::model::Entity::find_by_id(id)
            .one(db)
            .await
            .map_err(ApiError::database)?
        else {
            return Ok(()); // unknown id, let CrudCrate's update produce the 404
        };

        // Nothing here writes: `perform_update` carries the provenance flag in the row's own UPDATE,
        // so a request this hook goes on to reject leaves the chain treating the row exactly as it
        // did before.
        let stored_from = existing.valid_from;
        if let Some(Some(until)) = data.valid_until {
            let opens_at = match data.valid_from {
                Some(Some(patched)) => patched,
                _ => stored_from.with_timezone(&chrono::Utc),
            };
            if until <= opens_at {
                return Err(ApiError::bad_request(
                    "A calibration's end date must fall after its start date: a window that \
                     closes at or before it opens applies to no reading."
                        .to_string(),
                ));
            }
        }

        // Moving a curve's start onto another curve's start is the same collision as creating one
        // there.
        let Some(Some(new_from)) = data.valid_from else {
            return Ok(());
        };
        let sensor_id = existing.sensor_id;
        let parameter_id = existing.parameter_id;
        let parameter_id = match data.parameter_id {
            Some(patched) => patched,
            None => parameter_id,
        };

        if duplicate_instant_exists(db, sensor_id, parameter_id, new_from, Some(id)).await? {
            return Err(ApiError::bad_request(DUPLICATE_INSTANT.to_string()));
        }
        Ok(())
    }

    /// The default update, with the `valid_until_explicit` provenance carried in the same statement.
    ///
    /// The flag sits outside both CRUD models, so nothing writes it unless this hook does. Written
    /// from `before_update` instead, a request a later validation goes on to reject would still have
    /// moved the row onto the operator-window branch of `recompute_valid_until`, where `LEAST`
    /// ignores a NULL and the window can no longer reopen when the following curve is deleted. Set
    /// on the active model, a rejected update leaves the row exactly as it was.
    async fn perform_update(
        &self,
        db: &DatabaseConnection,
        id: Uuid,
        data: <SensorCalibration as CRUDResource>::UpdateModel,
    ) -> Result<SensorCalibration, ApiError> {
        use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel, Set};

        let provenance = valid_until_provenance(data.valid_until);
        let existing = super::model::Entity::find_by_id(id)
            .one(db)
            .await
            .map_err(ApiError::database)?
            .ok_or_else(|| ApiError::not_found("sensor_calibration", Some(id.to_string())))?;

        let mut active = data.merge_into_activemodel(existing.into_active_model())?;
        if let Some(explicit) = provenance {
            active.valid_until_explicit = Set(explicit);
        }
        let updated = active.update(db).await.map_err(ApiError::database)?;

        Ok(SensorCalibration::from(updated))
    }

    async fn after_create(
        &self,
        db: &DatabaseConnection,
        entity: &mut SensorCalibration,
    ) -> Result<(), ApiError> {
        reprocess_after_calibration_write(db, "calibration_create", entity.sensor_id, entity.id)
            .await
    }

    async fn after_update(
        &self,
        db: &DatabaseConnection,
        entity: &mut SensorCalibration,
    ) -> Result<(), ApiError> {
        reprocess_after_calibration_write(db, "calibration_update", entity.sensor_id, entity.id)
            .await
    }

    async fn perform_delete(&self, db: &DatabaseConnection, id: Uuid) -> Result<Uuid, ApiError> {
        let row = super::model::Entity::find_by_id(id)
            .one(db)
            .await
            .map_err(ApiError::database)?;

        let Some(row) = row else {
            return Err(ApiError::not_found(
                "sensor_calibration",
                Some(id.to_string()),
            ));
        };
        let sensor_id = row.sensor_id;

        if let Some(pinned) = pinned_readings(db, id).await? {
            return Err(ApiError::bad_request(format!(
                "Calibration {id} is pinned to {pinned} reading{plural}, so it cannot be deleted: \
                 a pin is attribution a person decided and the repoint leaves it alone. Roll the \
                 pin back first, then delete the curve.",
                plural = if pinned == 1 { "" } else { "s" }
            )));
        }

        // A curve that has corrected a reading is retired, never removed (Q107, M146): the row is
        // the provenance of every value it produced, and a delete would take that away and leave
        // the readings pointing at nothing. A curve nothing names has no history to keep.
        let used: i64 = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT count(*)::bigint AS n FROM readings WHERE calibration_id = $1",
                [id.into()],
            ))
            .await
            .map_err(ApiError::database)?
            .map(|r| r.try_get::<i64>("", "n"))
            .transpose()
            .map_err(ApiError::database)?
            .unwrap_or(0);
        if used > 0 {
            return Err(ApiError::bad_request(format!(
                "This calibration has corrected {used} reading(s), so it is retired rather than \
                 deleted: POST /api/sensor_calibrations/{id}/retire. Retiring moves those readings \
                 onto whatever else covers them, keeps the row and its provenance, and is \
                 reversible."
            )));
        }

        db.execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "DELETE FROM sensor_calibrations WHERE id = $1",
            [id.into()],
        ))
        .await
        .map_err(ApiError::database)?;

        reprocess_after_calibration_write(db, "calibration_delete", sensor_id, id).await?;

        Ok(id)
    }
}
