use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ActiveModelTrait, ConnectionTrait, EntityTrait, Statement, TransactionTrait};
use uuid::Uuid;

use super::model::SiteParameter;
use crate::routes::private::sensors::identity::require_measuring_instrument;

pub struct SiteParameterOperations;

impl CRUDOperations for SiteParameterOperations {
    type Resource = SiteParameter;

    /// The change-audit trigger reads the writer from the transaction, so the label is declared on
    /// every write this entity makes, before any hook or statement on it (B185).
    async fn after_begin<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
    ) -> Result<(), ApiError> {
        crate::common::actor::declare(db)
            .await
            .map_err(ApiError::database)
    }

    /// Retire everything the slot owns before it goes away: unattribute its readings and status
    /// events, delete the samples nothing references any more, release the streams that fed it,
    /// and rebuild the rollups. `retire_slot` also does the `data_streams` NULLing the foreign key
    /// requires, so the delete CrudCrate performs next succeeds.
    async fn before_delete<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
    ) -> Result<(), ApiError> {
        crate::routes::private::data_streams::views::retire_slot(
            db,
            crate::routes::private::data_streams::views::SlotScope::SiteParameter(id),
        )
        .await
        .map_err(|e| ApiError::internal(e.to_string(), None))?;
        Ok(())
    }

    /// A slot names the instrument that measures it, so the row it names has to be one something
    /// was measured on. A bookkeeping instrument stands in for a slot that has declared nothing,
    /// and declaring it would record the absence of an answer as an answer.
    async fn before_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        data: &<SiteParameter as CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        if let Some(sensor_id) = data.instrument_sensor_id {
            require_measuring_instrument(db, sensor_id, "the instrument that measures a slot")
                .await
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
        }
        Ok(())
    }

    /// The twin of `before_create`: the declaration is patchable, and the picker that sets it is
    /// the surface an operator reaches it through.
    async fn before_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        _id: Uuid,
        data: &<SiteParameter as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        if let Some(Some(sensor_id)) = data.instrument_sensor_id {
            require_measuring_instrument(db, sensor_id, "the instrument that measures a slot")
                .await
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
        }
        Ok(())
    }

    /// The insert and the name backfill, on the transaction the orchestrator opened.
    ///
    /// `name` is the slot's fulltext and sort key, so a row must never be visible without one, and
    /// a hook cannot supply it: the create model is immutable in `before_create` and `after_create`
    /// runs after the insert. Both statements go here, and the write they make is one because the
    /// lifecycle is one transaction; opening another here would only nest a savepoint inside it.
    async fn perform_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        data: <SiteParameter as CRUDResource>::CreateModel,
    ) -> Result<SiteParameter, ApiError> {
        let active: <SiteParameter as CRUDResource>::ActiveModelType = data.into();
        let model = active.insert(db).await.map_err(ApiError::database)?;
        let mut entity = SiteParameter::from(model);

        // A human-readable name from the parameter when the client omitted it.
        if entity.name.trim().is_empty()
            && let Some(parameter) =
                crate::routes::private::parameters::Entity::find_by_id(entity.parameter_id)
                    .one(db)
                    .await
                    .map_err(ApiError::database)?
        {
            db.execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE site_parameters SET name = $1 WHERE id = $2",
                [parameter.name.clone().into(), entity.id.into()],
            ))
            .await
            .map_err(ApiError::database)?;
            entity.name = parameter.name;
        }

        Ok(entity)
    }

    async fn after_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut SiteParameter,
    ) -> Result<(), ApiError> {
        // `is_active` and `is_public` defaults live in the model's `on_create`, so an omitted
        // field is already resolved by the time this hook runs and an explicit null stays null.
        // NOTE: alarm thresholds are intentionally NOT auto-created from parameter defaults.
        // Alarm evaluation already falls back to the parameter's `default_*` columns when no
        // `alarm_thresholds` row exists, so a default-valued row is redundant, and worse, a
        // site-specific copy would silently shadow a global threshold an operator set. A
        // site-specific row is created only when a user explicitly overrides via the editor.

        // Backfill derived values for the readings already present at this site when a
        // derived site_parameter is assigned. Enqueued as a durable `derived_assignment` job on
        // the claim-based worker pool. The guard skips the enqueue only when this definition's
        // own assignment or recompute is already in flight. An import or pairing backfill at any
        // site is not an overlap: rows it lands after this point reach the new slot through its
        // own derived cascade, and rows already present are this job's to compute.
        //
        // This stays a query rather than becoming the enqueue's `dedupe_key`: the key is released
        // when the worker claims the row, so it coalesces only while a job is queued, and the
        // skip has to hold while one is running too.
        if entity.entry_mode == "tool"
            && let Some(def_id) = definition_producing(db, entity.parameter_id).await?
        {
            let site_id = entity.site_id;

            let in_flight = db
                .query_one_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    r"SELECT 1
                      FROM reprocessing_jobs
                      WHERE status IN ('queued', 'pending', 'running', 'retrying')
                        AND trigger_type IN ('derived_assignment', 'derived_recompute')
                        AND trigger_id = $1
                      LIMIT 1",
                    [def_id.into()],
                ))
                .await
                .map_err(ApiError::database)?;

            if in_flight.is_some() {
                tracing::info!(
                    %def_id, %site_id,
                    "Skipping derived assignment backfill: this definition's backfill is already in flight"
                );
            } else {
                crate::routes::private::reprocessing_jobs::worker::enqueue(
                    db,
                    "derived_assignment",
                    None,
                    Some(def_id),
                    &serde_json::json!({ "derived_definition_id": def_id, "site_id": site_id }),
                    None,
                )
                .await
                .map_err(ApiError::database)?;
            }
        }

        Ok(())
    }

    // Breach evaluation only considers slots whose site_parameter is active, so toggling
    // `is_active` (or removing the slot) can open or resolve alarms with no new reading.
    // Reconcile immediately instead of waiting for the backstop sweep.
    async fn after_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        _entity: &mut SiteParameter,
    ) -> Result<(), ApiError> {
        crate::routes::private::alarms::sweeper::reconcile_all_from_hook(db).await;
        Ok(())
    }

    async fn after_delete<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        _id: Uuid,
    ) -> Result<(), ApiError> {
        crate::routes::private::alarms::sweeper::reconcile_all_from_hook(db).await;
        Ok(())
    }
}

/// The definition that produces a parameter, if one does. A calculation names the parameter it
/// outputs, and an output has exactly one producer (`idx_derived_definitions_output_parameter`),
/// so the slot needs no reference of its own.
async fn definition_producing<C: ConnectionTrait>(
    db: &C,
    parameter_id: Uuid,
) -> Result<Option<Uuid>, ApiError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id FROM calculation_formulas WHERE output_parameter_id = $1 LIMIT 1",
            [parameter_id.into()],
        ))
        .await
        .map_err(ApiError::database)?;
    row.map(|r| r.try_get::<Uuid>("", "id"))
        .transpose()
        .map_err(ApiError::database)
}
