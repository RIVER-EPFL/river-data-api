use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{
    ActiveModelTrait, ConnectionTrait, DatabaseConnection, EntityTrait, Statement, TransactionTrait,
};
use uuid::Uuid;

use super::model::SiteParameter;

pub struct SiteParameterOperations;

#[async_trait]
impl CRUDOperations for SiteParameterOperations {
    type Resource = SiteParameter;

    /// One row at a time, through the single-row path: the crudcrate default delegates to the
    /// resource, which delegates back here, and the single-row hooks are what a batch needs too.
    async fn update_many(
        &self,
        db: &DatabaseConnection,
        updates: Vec<(Uuid, <SiteParameter as CRUDResource>::UpdateModel)>,
    ) -> Result<Vec<SiteParameter>, ApiError> {
        let mut updated = Vec::with_capacity(updates.len());
        for (id, data) in updates {
            updated.push(self.update(db, id, data).await?);
        }
        Ok(updated)
    }

    /// One row at a time, through the single-row path: the crudcrate default `create_many`
    /// delegates to the resource, which delegates back here, so the default recurses; the loop
    /// also runs the single-row hooks for every item.
    async fn create_many(
        &self,
        db: &DatabaseConnection,
        data: Vec<<SiteParameter as CRUDResource>::CreateModel>,
    ) -> Result<Vec<SiteParameter>, ApiError> {
        let mut created = Vec::with_capacity(data.len());
        for item in data {
            created.push(self.create(db, item).await?);
        }
        Ok(created)
    }

    /// Retire everything the slot owns before it goes away: unattribute its readings and status
    /// events, delete the samples nothing references any more, release the streams that fed it,
    /// and rebuild the rollups. `retire_slot` also does the `data_streams` NULLing the foreign key
    /// requires, so the delete CrudCrate performs next succeeds.
    async fn before_delete(&self, db: &DatabaseConnection, id: Uuid) -> Result<(), ApiError> {
        crate::routes::private::data_streams::views::retire_slot(
            db,
            crate::routes::private::data_streams::views::SlotScope::SiteParameter(id),
        )
        .await
        .map_err(|e| ApiError::internal(e.to_string(), None))?;
        Ok(())
    }

    /// The bulk delete takes the same teardown per slot; without it a multi-slot delete leaves
    /// readings attributed to a slot that no longer exists and fails on the stream foreign key.
    async fn before_delete_many(
        &self,
        db: &DatabaseConnection,
        ids: &[Uuid],
    ) -> Result<(), ApiError> {
        for id in ids {
            crate::routes::private::data_streams::views::retire_slot(
                db,
                crate::routes::private::data_streams::views::SlotScope::SiteParameter(*id),
            )
            .await
            .map_err(|e| ApiError::internal(e.to_string(), None))?;
        }
        Ok(())
    }

    /// The insert and the name backfill are one transaction.
    ///
    /// `name` is the slot's fulltext and sort key, so a row must never be visible without one, and
    /// a hook cannot supply it: the create model is immutable in `before_create` and `after_create`
    /// runs after the insert has committed. Both statements go here, on one transaction.
    async fn perform_create(
        &self,
        db: &DatabaseConnection,
        data: <SiteParameter as CRUDResource>::CreateModel,
    ) -> Result<SiteParameter, ApiError> {
        let txn = db.begin().await.map_err(ApiError::database)?;
        let active: <SiteParameter as CRUDResource>::ActiveModelType = data.into();
        let model = active.insert(&txn).await.map_err(ApiError::database)?;
        let mut entity = SiteParameter::from(model);

        // A human-readable name from the parameter when the client omitted it.
        if entity.name.trim().is_empty()
            && let Some(parameter) =
                crate::routes::private::parameters::Entity::find_by_id(entity.parameter_id)
                    .one(&txn)
                    .await
                    .map_err(ApiError::database)?
        {
            txn.execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE site_parameters SET name = $1 WHERE id = $2",
                [parameter.name.clone().into(), entity.id.into()],
            ))
            .await
            .map_err(ApiError::database)?;
            entity.name = parameter.name;
        }

        txn.commit().await.map_err(ApiError::database)?;
        Ok(entity)
    }

    async fn after_create(
        &self,
        db: &DatabaseConnection,
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
    async fn after_update(
        &self,
        db: &DatabaseConnection,
        _entity: &mut SiteParameter,
    ) -> Result<(), ApiError> {
        crate::routes::private::alarms::sweeper::reconcile_all_from_hook(db).await;
        Ok(())
    }

    async fn after_delete(&self, db: &DatabaseConnection, _id: Uuid) -> Result<(), ApiError> {
        crate::routes::private::alarms::sweeper::reconcile_all_from_hook(db).await;
        Ok(())
    }

    async fn after_delete_many(
        &self,
        db: &DatabaseConnection,
        _ids: &[Uuid],
    ) -> Result<(), ApiError> {
        crate::routes::private::alarms::sweeper::reconcile_all_from_hook(db).await;
        Ok(())
    }
}

/// The definition that produces a parameter, if one does. A calculation names the parameter it
/// outputs, and an output has exactly one producer (`idx_derived_definitions_output_parameter`),
/// so the slot needs no reference of its own.
async fn definition_producing(
    db: &DatabaseConnection,
    parameter_id: Uuid,
) -> Result<Option<Uuid>, ApiError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id FROM derived_parameter_definitions WHERE output_parameter_id = $1 LIMIT 1",
            [parameter_id.into()],
        ))
        .await
        .map_err(ApiError::database)?;
    row.map(|r| r.try_get::<Uuid>("", "id"))
        .transpose()
        .map_err(ApiError::database)
}
