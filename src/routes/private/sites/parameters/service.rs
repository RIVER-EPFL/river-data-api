use std::collections::HashMap;

use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter,
    QuerySelect, TransactionTrait,
};
use uuid::Uuid;

use super::models::{
    CatalogParameter, Column, Entity, GroupMember, Model as SiteParameterModel, SiteParameter,
    SlotDescriptor,
};
use crate::error::AppResult;
use crate::routes::private::parameters;
use crate::routes::private::parameters::derived::definition_model as calculation_formulas;
use crate::routes::private::reprocessing_jobs::model as reprocessing_jobs;
use crate::routes::private::sensors::service::require_measuring_instrument;

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
        crate::routes::private::data_streams::flows::retire_slot(
            db,
            crate::routes::private::data_streams::models::SlotScope::SiteParameter(id),
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
            Entity::update_many()
                .col_expr(Column::Name, Expr::value(parameter.name.clone()))
                .filter(Column::Id.eq(entity.id))
                .exec(db)
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

            let in_flight = reprocessing_jobs::Entity::find()
                .filter(
                    reprocessing_jobs::Column::Status
                        .is_in(["queued", "pending", "running", "retrying"]),
                )
                .filter(
                    reprocessing_jobs::Column::TriggerType
                        .is_in(["derived_assignment", "derived_recompute"]),
                )
                .filter(reprocessing_jobs::Column::TriggerId.eq(def_id))
                .select_only()
                .column(reprocessing_jobs::Column::Id)
                .into_tuple::<Uuid>()
                .one(db)
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
        crate::routes::private::alarms::flows::reconcile_all_from_hook(db).await;
        Ok(())
    }

    async fn after_delete<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        _id: Uuid,
    ) -> Result<(), ApiError> {
        crate::routes::private::alarms::flows::reconcile_all_from_hook(db).await;
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
    calculation_formulas::Entity::find()
        .filter(calculation_formulas::Column::OutputParameterId.eq(parameter_id))
        .select_only()
        .column(calculation_formulas::Column::Id)
        .into_tuple::<Uuid>()
        .one(db)
        .await
        .map_err(ApiError::database)
}

/// Load the catalog rows for a set of parameter ids, keyed by parameter id.
pub async fn catalog_map(
    db: &DatabaseConnection,
    parameter_ids: impl IntoIterator<Item = Uuid>,
) -> AppResult<HashMap<Uuid, CatalogParameter>> {
    let ids: Vec<Uuid> = parameter_ids.into_iter().collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = parameters::Entity::find()
        .filter(parameters::Column::Id.is_in(ids))
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|p| {
            (
                p.id,
                CatalogParameter {
                    code: p.code,
                    name: p.name,
                    default_units: p.default_units,
                },
            )
        })
        .collect())
}

impl SlotDescriptor {
    /// Resolve one slot against its catalog row (`None` when the parameter is missing from the
    /// catalog, which leaves the code empty and the units unresolved).
    pub fn resolve(slot: &SiteParameterModel, catalog: Option<&CatalogParameter>) -> Self {
        let catalog_name = catalog.map(|c| c.name.clone());
        let units = slot.display_units.clone().or_else(|| {
            catalog
                .map(|c| c.default_units.clone())
                .filter(|u| !u.is_empty())
        });
        Self {
            id: slot.id,
            parameter_id: slot.parameter_id,
            code: catalog.map(|c| c.code.clone()).unwrap_or_default(),
            name: catalog_name.clone().unwrap_or_else(|| slot.name.clone()),
            catalog_name,
            slot_name: slot.name.clone(),
            sensor_type: if slot.sensor_type.is_empty() {
                slot.name.clone()
            } else {
                slot.sensor_type.clone()
            },
            units,
            display_units: slot.display_units.clone(),
            decimal_places: slot.decimal_places,
        }
    }

    /// Resolve a batch of slots against one catalog map, preserving input order.
    pub fn resolve_all(
        slots: &[SiteParameterModel],
        catalog: &HashMap<Uuid, CatalogParameter>,
    ) -> Vec<Self> {
        slots
            .iter()
            .map(|s| Self::resolve(s, catalog.get(&s.parameter_id)))
            .collect()
    }
}

/// The group's members and the site's slots, decided in one place so the route and its test agree
/// on what "already carried" means: a slot exists for a member when the site holds a row for that
/// catalog parameter, whatever the group says about it.
#[must_use]
pub fn partition_members(
    members: &[GroupMember],
    held: &std::collections::HashSet<Uuid>,
) -> (Vec<GroupMember>, Vec<GroupMember>) {
    let mut create = Vec::new();
    let mut existing = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for member in members {
        if held.contains(&member.0) {
            existing.push(member.clone());
        } else if seen.insert(member.0) {
            create.push(member.clone());
        }
    }
    (create, existing)
}

/// SQL selecting the slots a retag names, by slot id (`$2`) or through a stream's pairing (`$3`).
pub const SLOT_SCOPE: &str = "EXISTS (SELECT 1 FROM site_parameters sp \
     WHERE sp.site_id = s.site_id AND sp.parameter_id = s.parameter_id \
       AND (sp.id = ANY($2) \
            OR EXISTS (SELECT 1 FROM data_streams ds \
                       WHERE ds.id = ANY($3) AND ds.site_parameter_id = sp.id)))";

#[cfg(test)]
#[path = "tests/service.rs"]
mod tests;
