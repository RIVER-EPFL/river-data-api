use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, EntityTrait, TransactionTrait};
use uuid::Uuid;

use super::models::Constant;
use crate::routes::private::reprocessing_jobs::service as jobs;
use crate::routes::private::tools::service as tools;

/// A constant is an input to every calculation that declares it, so changing its value changes what
/// every stored output would produce today. A constant carries no versions, so there is no arm that
/// leaves old readings on the old value: the edit enqueues `event_recompute` scoped to the constant
/// (Q170), which repairs exactly the visits whose stored provenance names it, and a
/// `derived_recompute` of each formula calculation reading it, for the values a stream pass made.
pub struct ConstantOperations;

/// The stored value and name, for an update that has not happened yet.
async fn stored<C: ConnectionTrait>(db: &C, id: Uuid) -> Result<Option<(f64, String)>, ApiError> {
    let row = super::models::Entity::find_by_id(id)
        .one(db)
        .await
        .map_err(ApiError::from)?;
    Ok(row.map(|r| (r.value, r.name)))
}

#[must_use]
pub fn recompute_dedupe_key(name: &str) -> String {
    format!("event_recompute:constant:{name}")
}

impl CRUDOperations for ConstantOperations {
    type Resource = Constant;

    /// The change-audit trigger reads the writer from the transaction, so the label is declared on
    /// every write this entity makes, before any hook or statement on it.
    async fn after_begin<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
    ) -> Result<(), ApiError> {
        crate::common::actor::declare(db)
            .await
            .map_err(ApiError::database)
    }

    /// A value that moves invalidates every stored output computed from it, so the recompute is
    /// enqueued on the transaction the edit runs in and commits or rolls back with it. A units or
    /// description edit changes no calculation and recomputes nothing.
    async fn before_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
        data: &<Constant as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        let Some(Some(value)) = data.value else {
            return Ok(());
        };
        let Some((previous, stored_name)) = stored(db, id).await? else {
            return Ok(());
        };
        if previous == value {
            return Ok(());
        }
        let name = data.name.clone().flatten().unwrap_or(stored_name);
        let calculations = tools::calculations_reading_constant(db, &name)
            .await
            .map_err(|e| ApiError::internal(e.to_string(), None))?;
        for job in tools::constant_edit_jobs(id, &name, previous, value, &calculations) {
            if let Err(e) = jobs::enqueue(
                db,
                job.kind,
                None,
                job.trigger_id,
                &job.params,
                Some(&job.dedupe_key),
            )
            .await
            {
                tracing::warn!(error = %e, constant = %name, kind = job.kind, "constants: failed to enqueue recompute");
            }
        }
        Ok(())
    }
}
