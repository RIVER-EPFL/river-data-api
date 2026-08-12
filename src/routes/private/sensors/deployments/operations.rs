use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use super::model::SensorDeployment;
use crate::routes::private::sensors::calibrations::service::recompute_deployed_until;

pub struct SensorDeploymentOperations;

/// Spawn a tracked reprocess for a deployment change. Runs the **slot-scoped** reprocess first
/// (`reprocess_site_parameter_readings`) so a backdated/edited window re-attributes the affected
/// (site, parameter) by deployment timeline, stamping `sensor_id` onto previously unattributed
/// (NULL-sensor) history, then a per-sensor pass to reconcile the sensor's own rows at any vacated
/// slot. `parameter_id` is the deployment's authored parameter (passed through to the job).
async fn spawn_slot_reprocess(
    db: &DatabaseConnection,
    sensor_id: Uuid,
    site_id: Uuid,
    parameter_id: Uuid,
    trigger_type: &str,
    trigger_id: Uuid,
) -> Result<(), sea_orm::DbErr> {
    crate::routes::private::reprocessing_jobs::worker::enqueue(
        db,
        trigger_type,
        Some(sensor_id),
        Some(trigger_id),
        &serde_json::json!({ "sensor_id": sensor_id, "site_id": site_id, "parameter_id": parameter_id }),
        None,
    )
    .await?;
    Ok(())
}

#[async_trait]
impl CRUDOperations for SensorDeploymentOperations {
    type Resource = SensorDeployment;

    async fn before_create(
        &self,
        db: &DatabaseConnection,
        data: &<SensorDeployment as CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        // Recall is scoped to the SAME parameter (channel): a multi-channel instrument holds one open
        // deployment per parameter, so deploying its temperature channel must not close its still-live
        // conductivity channel. A same-parameter move across sites still closes the old site's row.
        let result = db
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"UPDATE sensor_deployments
                  SET deployed_until = $1
                  WHERE sensor_id = $2 AND parameter_id = $3 AND deployed_until IS NULL",
                [
                    data.deployed_from.into(),
                    data.sensor_id.into(),
                    data.parameter_id.into(),
                ],
            ))
            .await
            .map_err(ApiError::database)?;

        if result.rows_affected() > 0 {
            tracing::info!(
                sensor_id = %data.sensor_id,
                parameter_id = %data.parameter_id,
                recalled = result.rows_affected(),
                "Auto-recalled active deployment(s) for this parameter on new deploy"
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
                    AND d.parameter_id = $5
                    AND d.sensor_id <> $2
                    AND tstzrange(d.deployed_from, COALESCE(d.deployed_until, 'infinity'::timestamptz), '[)')
                        && tstzrange($3, COALESCE($4, 'infinity'::timestamptz), '[)')
                  LIMIT 1",
                [
                    data.site_id.into(),
                    data.sensor_id.into(),
                    data.deployed_from.into(),
                    data.deployed_until.into(),
                    data.parameter_id.into(),
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

    // Mirror of `before_create` for edits: a PATCH that moves a deployment's window/site into
    // another sensor's slot would otherwise hit `excl_deployment_site_param_slot` as a raw 500.
    // Pre-check (excluding the row being edited) so the operator gets a clear 400, and auto-recall
    // the sensor's other open deployments when this edit keeps/makes it open. The recall and the
    // CrudCrate-applied UPDATE are separate statements (hooks don't share the update's txn), so the
    // EXCLUDE constraint remains the atomic backstop and `after_update`'s recompute re-chains the
    // sensor's own timeline.
    async fn before_update(
        &self,
        db: &DatabaseConnection,
        id: Uuid,
        data: &<SensorDeployment as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        // Only the slot-defining fields can create an overlap. If none were patched (e.g. notes or
        // deployment_type only), there's nothing to re-enforce.
        if data.site_id.is_none()
            && data.sensor_id.is_none()
            && data.deployed_from.is_none()
            && data.deployed_until.is_none()
        {
            return Ok(());
        }

        let Some(existing) = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT sensor_id, site_id, parameter_id, deployed_from, deployed_until \
                 FROM sensor_deployments WHERE id = $1",
                [id.into()],
            ))
            .await
            .map_err(ApiError::database)?
        else {
            return Ok(()); // unknown id, let CrudCrate's update produce the 404
        };

        // parameter_id is immutable on update (exclude(update)); the slot's parameter is the row's own.
        let cur_param: Uuid = existing.try_get("", "parameter_id").map_err(ApiError::database)?;
        let cur_sensor: Uuid = existing.try_get("", "sensor_id").map_err(ApiError::database)?;
        let cur_site: Uuid = existing.try_get("", "site_id").map_err(ApiError::database)?;
        let cur_from: chrono::DateTime<chrono::FixedOffset> =
            existing.try_get("", "deployed_from").map_err(ApiError::database)?;
        let cur_until: Option<chrono::DateTime<chrono::FixedOffset>> =
            existing.try_get("", "deployed_until").map_err(ApiError::database)?;

        // Merge the double-option patch over the existing row (outer None = field absent).
        let new_sensor = match data.sensor_id {
            Some(Some(v)) => v,
            _ => cur_sensor,
        };
        let new_site = match data.site_id {
            Some(Some(v)) => v,
            _ => cur_site,
        };
        let new_from: chrono::DateTime<chrono::Utc> = match data.deployed_from {
            Some(Some(v)) => v,
            _ => cur_from.with_timezone(&chrono::Utc),
        };
        let new_until: Option<chrono::DateTime<chrono::Utc>> = match data.deployed_until {
            Some(inner) => inner,
            None => cur_until.map(|t| t.with_timezone(&chrono::Utc)),
        };

        let conflict = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT 1 FROM sensor_deployments d
                  WHERE d.site_id = $1
                    AND d.parameter_id = $6
                    AND d.sensor_id <> $2
                    AND d.id <> $3
                    AND tstzrange(d.deployed_from, COALESCE(d.deployed_until, 'infinity'::timestamptz), '[)')
                        && tstzrange($4, COALESCE($5, 'infinity'::timestamptz), '[)')
                  LIMIT 1",
                [
                    new_site.into(),
                    new_sensor.into(),
                    id.into(),
                    new_from.into(),
                    new_until.into(),
                    cur_param.into(),
                ],
            ))
            .await
            .map_err(ApiError::database)?;

        if conflict.is_some() {
            return Err(ApiError::bad_request(
                "Another sensor is already deployed to this site for this parameter over an \
                 overlapping period. Recall it first, then move this deployment."
                    .to_string(),
            ));
        }

        // If the edit keeps/makes this deployment open-ended, close the sensor's other open
        // deployments at the new start (twin of the before_create recall, excluding self).
        if new_until.is_none() {
            let recalled = db
                .execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    // Same-parameter scope as before_create: don't close other channels of a
                    // multi-channel instrument.
                    r"UPDATE sensor_deployments SET deployed_until = $1
                      WHERE sensor_id = $2 AND parameter_id = $4 AND deployed_until IS NULL AND id <> $3",
                    [new_from.into(), new_sensor.into(), id.into(), cur_param.into()],
                ))
                .await
                .map_err(ApiError::database)?;
            if recalled.rows_affected() > 0 {
                tracing::info!(
                    sensor_id = %new_sensor,
                    recalled = recalled.rows_affected(),
                    "Auto-recalled active deployment(s) on deployment edit"
                );
            }
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

        spawn_slot_reprocess(
            db,
            entity.sensor_id,
            entity.site_id,
            entity.parameter_id,
            "deployment_create",
            entity.id,
        )
        .await
        .map_err(ApiError::database)?;

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

        spawn_slot_reprocess(
            db,
            entity.sensor_id,
            entity.site_id,
            entity.parameter_id,
            "deployment_update",
            entity.id,
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
                "SELECT sensor_id, site_id, parameter_id FROM sensor_deployments WHERE id = $1",
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
        let site_id: Uuid = row
            .try_get("", "site_id")
            .map_err(ApiError::database)?;
        let parameter_id: Uuid = row
            .try_get("", "parameter_id")
            .map_err(ApiError::database)?;

        // readings.deployment_id has no ON DELETE action, clear references first, then delete.
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

        spawn_slot_reprocess(db, sensor_id, site_id, parameter_id, "deployment_delete", id)
            .await
            .map_err(ApiError::database)?;

        Ok(id)
    }
}
