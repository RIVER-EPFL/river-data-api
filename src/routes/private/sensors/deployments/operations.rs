use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use super::model::SensorDeployment;
use super::slots::{self, SlotRequest};
use crate::routes::private::sensors::calibrations::service::recompute_deployed_until;

pub struct SensorDeploymentOperations;

/// A window whose end precedes its start is not a range Postgres can store, and the exclusion
/// constraint would raise a raw error on it.
fn reject_inverted_window(
    deployed_from: chrono::DateTime<chrono::Utc>,
    deployed_until: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<(), ApiError> {
    match deployed_until {
        Some(until) if until < deployed_from => Err(ApiError::bad_request(format!(
            "deployed_until ({}) precedes deployed_from ({})",
            until.to_rfc3339(),
            deployed_from.to_rfc3339()
        ))),
        _ => Ok(()),
    }
}

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
        reject_inverted_window(data.deployed_from, data.deployed_until)?;

        // Every rejection comes before the recall. A refused create must leave the sensor deployed
        // exactly where it was: the recall used to run first, so a 400 still closed the open
        // deployment and the next reprocess un-attributed everything logged after it.
        //
        // One sensor per (site, parameter) at a time is hard-enforced by the
        // `excl_deployment_site_param_slot` constraint. The check names the blocking row so the
        // operator gets an actionable 400 instead of a raw constraint violation; the constraint
        // remains the atomic backstop. Rows this write is about to recall are not blocking, and the
        // constraint carries no sensor term, so a same-sensor historical overlap is.
        let occupant = slots::find_occupant(
            db,
            &SlotRequest {
                site_id: data.site_id,
                parameter_id: data.parameter_id,
                deployed_from: data.deployed_from,
                deployed_until: data.deployed_until,
                exclude_deployment: None,
                recalled_sensor: Some(data.sensor_id),
            },
        )
        .await
        .map_err(ApiError::database)?;

        if let Some(occupant) = occupant {
            return Err(ApiError::bad_request(slots::conflict_message(
                &occupant,
                data.sensor_id,
                "deploy",
            )));
        }

        let recalled = slots::recall_open_deployments(
            db,
            data.sensor_id,
            data.parameter_id,
            data.deployed_from,
            None,
        )
        .await
        .map_err(ApiError::database)?;
        if recalled > 0 {
            tracing::info!(
                sensor_id = %data.sensor_id,
                parameter_id = %data.parameter_id,
                recalled,
                "Auto-recalled active deployment(s) for this parameter on new deploy"
            );
        }

        Ok(())
    }

    // Mirror of `before_create` for edits: a PATCH that moves a deployment's window/site into
    // another sensor's slot would otherwise hit `excl_deployment_site_param_slot` as a raw 500.
    // Every rejection comes first (excluding the row being edited from the slot check), then the
    // boundary follow, then the auto-recall of the sensor's other open deployments when this edit
    // keeps/makes it open. The recall and the CrudCrate-applied UPDATE are separate statements
    // (hooks don't share the update's txn), so the EXCLUDE constraint remains the atomic backstop
    // and `after_update`'s recompute re-chains the sensor's own timeline.
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

        reject_inverted_window(new_from, new_until)?;

        let occupant = slots::find_occupant(
            db,
            &SlotRequest {
                site_id: new_site,
                parameter_id: cur_param,
                deployed_from: new_from,
                deployed_until: new_until,
                exclude_deployment: Some(id),
                // The recall below only runs when the edit leaves this deployment open-ended.
                recalled_sensor: new_until.is_none().then_some(new_sensor),
            },
        )
        .await
        .map_err(ApiError::database)?;

        if let Some(occupant) = occupant {
            return Err(ApiError::bad_request(slots::conflict_message(
                &occupant,
                new_sensor,
                "move this deployment",
            )));
        }

        // A move date corrected forward hands the vacated period back to where the instrument
        // actually was; `recompute_deployed_until` cannot, it only shortens.
        let followed = slots::follow_forward_move(
            db,
            cur_sensor,
            cur_param,
            id,
            cur_from.with_timezone(&chrono::Utc),
            new_from,
        )
        .await
        .map_err(ApiError::database)?;
        if followed > 0 {
            tracing::info!(
                sensor_id = %cur_sensor,
                parameter_id = %cur_param,
                followed,
                "Adjacent deployment's end date followed the corrected move date"
            );
        }

        // If the edit keeps/makes this deployment open-ended, close the sensor's other open
        // deployments at the new start (twin of the before_create recall, excluding self).
        if new_until.is_none() {
            let recalled =
                slots::recall_open_deployments(db, new_sensor, cur_param, new_from, Some(id))
                    .await
                    .map_err(ApiError::database)?;
            if recalled > 0 {
                tracing::info!(
                    sensor_id = %new_sensor,
                    recalled,
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
        //
        // `deployment_id` is neither the segmentby nor the time dimension, so no compressed batch can
        // be excluded by metadata: the clear goes through the guarded writer, which lifts the
        // decompression cap it would otherwise hit on a sensor with historical readings.
        crate::common::bulk_write::guarded_mutation(
            db,
            Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE readings SET deployment_id = NULL WHERE deployment_id = $1",
                [id.into()],
            ),
        )
        .await
        .map_err(|e| {
            ApiError::internal(
                "Failed to clear the deployment's readings references",
                Some(e.to_string()),
            )
        })?;

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
