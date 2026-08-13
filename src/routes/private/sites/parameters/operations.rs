use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, EntityTrait, Order, QueryFilter,
    QueryOrder, QuerySelect, Statement,
};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use super::model::{Column, Entity, SiteParameter};

async fn enrich(db: &DatabaseConnection, items: &mut [SiteParameter]) -> Result<(), ApiError> {
    if items.is_empty() {
        return Ok(());
    }

    let param_ids: HashSet<Uuid> = items.iter().map(|sp| sp.parameter_id).collect();
    let def_ids: HashSet<Uuid> = items
        .iter()
        .filter_map(|sp| sp.derived_definition_id)
        .collect();

    if !param_ids.is_empty() {
        let params = crate::routes::private::parameters::Entity::find()
            .filter(crate::routes::private::parameters::Column::Id.is_in(param_ids))
            .all(db)
            .await
            .map_err(ApiError::database)?;
        let by_id: HashMap<Uuid, crate::routes::private::parameters::Parameter> = params
            .into_iter()
            .map(|m| (m.id, crate::routes::private::parameters::Parameter::from(m)))
            .collect();
        for sp in items.iter_mut() {
            if let Some(p) = by_id.get(&sp.parameter_id) {
                sp.parameter = vec![p.clone()];
            }
        }
    }

    if !def_ids.is_empty() {
        let defs = crate::routes::private::parameters::derived::definition_model::Entity::find()
            .filter(
                crate::routes::private::parameters::derived::definition_model::Column::Id
                    .is_in(def_ids),
            )
            .all(db)
            .await
            .map_err(ApiError::database)?;
        let by_id: HashMap<Uuid, crate::routes::private::parameters::derived::definition_model::DerivedParameterDefinition> = defs
            .into_iter()
            .map(|m| (m.id, crate::routes::private::parameters::derived::definition_model::DerivedParameterDefinition::from(m)))
            .collect();
        for sp in items.iter_mut() {
            if let Some(def_id) = sp.derived_definition_id
                && let Some(def) = by_id.get(&def_id)
            {
                sp.derived_definition = Some(def.clone());
            }
        }
    }

    Ok(())
}

pub struct SiteParameterOperations;

#[async_trait]
impl CRUDOperations for SiteParameterOperations {
    type Resource = SiteParameter;

    async fn fetch_one(
        &self,
        db: &DatabaseConnection,
        id: Uuid,
    ) -> Result<SiteParameter, ApiError> {
        let model = Entity::find_by_id(id)
            .one(db)
            .await
            .map_err(ApiError::database)?
            .ok_or_else(|| {
                ApiError::not_found(SiteParameter::RESOURCE_NAME_SINGULAR, Some(id.to_string()))
            })?;
        let mut resource = SiteParameter::from(model);
        let mut slice = std::slice::from_mut(&mut resource);
        enrich(db, &mut slice).await?;
        Ok(resource)
    }

    async fn fetch_all(
        &self,
        db: &DatabaseConnection,
        condition: &Condition,
        order_column: Column,
        order_direction: Order,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<<SiteParameter as CRUDResource>::ListModel>, ApiError> {
        let models = Entity::find()
            .filter(condition.clone())
            .order_by(order_column, order_direction)
            .offset(offset)
            .limit(limit)
            .all(db)
            .await
            .map_err(ApiError::database)?;
        let mut resources: Vec<SiteParameter> =
            models.into_iter().map(SiteParameter::from).collect();
        enrich(db, &mut resources).await?;
        Ok(resources
            .into_iter()
            .map(<SiteParameter as CRUDResource>::ListModel::from)
            .collect())
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

    async fn after_create(
        &self,
        db: &DatabaseConnection,
        entity: &mut SiteParameter,
    ) -> Result<(), ApiError> {
        // `is_active` and `is_public` defaults live in the model's `on_create`, so an omitted
        // field is already resolved by the time this hook runs and an explicit null stays null.
        let parameter = crate::routes::private::parameters::Entity::find_by_id(entity.parameter_id)
            .one(db)
            .await
            .map_err(ApiError::database)?;

        let Some(parameter) = parameter else {
            return Ok(());
        };

        // Backfill a human-readable name from the parameter when the client omitted it.
        // `name` is fulltext/sortable, so it must not be left empty.
        if entity.name.trim().is_empty() {
            db.execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE site_parameters SET name = $1 WHERE id = $2",
                [parameter.name.clone().into(), entity.id.into()],
            ))
            .await
            .map_err(ApiError::database)?;
            entity.name = parameter.name.clone();
        }

        // NOTE: alarm thresholds are intentionally NOT auto-created from parameter defaults.
        // Alarm evaluation already falls back to the parameter's `default_*` columns when no
        // `alarm_thresholds` row exists, so a default-valued row is redundant, and worse, a
        // site-specific copy would silently shadow a global threshold an operator set. A
        // site-specific row is created only when a user explicitly overrides via the editor.

        // Backfill derived values for the readings already present at this site when a
        // derived site_parameter is assigned. Enqueued as a durable `derived_assignment` job on
        // the claim-based worker pool. A guard skips the enqueue if an overlapping backfill is
        // already in flight (this definition's own assignment/recompute, or a CSV import / pairing
        // backfill that will produce the same derived rows) so we don't double-run the same work.
        if entity.is_derived == Some(true)
            && let Some(def_id) = entity.derived_definition_id
        {
            let site_id = entity.site_id;

            let in_flight = db
                .query_one(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    r"SELECT 1
                      FROM reprocessing_jobs
                      WHERE status IN ('queued', 'pending', 'running', 'retrying')
                        AND (
                          (trigger_type IN ('derived_assignment', 'derived_recompute') AND trigger_id = $1)
                          OR trigger_type IN ('csv_import', 'pairing_backfill')
                        )
                      LIMIT 1",
                    [def_id.into()],
                ))
                .await
                .map_err(ApiError::database)?;

            if in_flight.is_some() {
                tracing::info!(
                    %def_id, %site_id,
                    "Skipping derived assignment backfill: an overlapping reprocessing job is in flight"
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
