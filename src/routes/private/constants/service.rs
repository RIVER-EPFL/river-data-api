use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, EntityTrait, TransactionTrait};
use uuid::Uuid;

use super::models::Constant;
use crate::routes::private::reprocessing_jobs::service as jobs;

/// A constant is an input to every calculation that declares it, so changing its value changes what
/// every stored output would produce today. A constant carries no versions, so there is no arm that
/// leaves old readings on the old value: the edit enqueues `event_recompute` scoped to the constant
/// (Q170), which repairs exactly the visits whose stored provenance names it.
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
        let key = recompute_dedupe_key(&name);
        // Both values travel on the job row: the ledger rows the recompute writes name the run
        // that moved them, so the run has to say what the move was.
        if let Err(e) = jobs::enqueue(
            db,
            "event_recompute",
            None,
            Some(id),
            &serde_json::json!({
                "constant": name,
                "previous_value": previous,
                "value": value,
            }),
            Some(&key),
        )
        .await
        {
            tracing::warn!(error = %e, constant = %name, "constants: failed to enqueue recompute");
        }
        Ok(())
    }
}
