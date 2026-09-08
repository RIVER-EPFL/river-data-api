use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, Statement, TransactionTrait};
use uuid::Uuid;

use super::model::Constant;
use crate::routes::private::reprocessing_jobs::worker;

/// A constant is an input to every calculation that declares it, so changing its value changes what
/// every stored output would produce today. Nothing is rewritten by the save: the edit enqueues the
/// report-only `event_audit`, which files a `stale_output` finding per disagreement, and repair
/// stays the scoped `event_recompute` a person asks for.
pub struct ConstantOperations;

/// The stored value and name, for an update that has not happened yet.
async fn stored<C: ConnectionTrait>(db: &C, id: Uuid) -> Result<Option<(f64, String)>, ApiError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT value, name FROM constants WHERE id = $1",
            [id.into()],
        ))
        .await
        .map_err(ApiError::from)?;
    row.map(|r| {
        Ok((
            r.try_get::<f64>("", "value")?,
            r.try_get::<String>("", "name")?,
        ))
    })
    .transpose()
}

#[must_use]
pub fn audit_dedupe_key(name: &str) -> String {
    format!("event_audit:constant:{name}")
}

impl CRUDOperations for ConstantOperations {
    type Resource = Constant;

    /// A value that moves invalidates every stored output computed from it, so the audit is
    /// enqueued on the transaction the edit runs in and commits or rolls back with it. A units or
    /// description edit changes no calculation and audits nothing.
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
        let key = audit_dedupe_key(&name);
        if let Err(e) = worker::enqueue(
            db,
            "event_audit",
            None,
            Some(id),
            &serde_json::json!({ "constant": name }),
            Some(&key),
        )
        .await
        {
            tracing::warn!(error = %e, constant = %name, "constants: failed to enqueue audit");
        }
        Ok(())
    }
}
