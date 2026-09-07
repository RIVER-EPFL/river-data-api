use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use super::model::Constant;
use crate::routes::private::reprocessing_jobs::worker;

/// A constant is an input to every calculation that declares it, so changing its value changes what
/// every stored output would produce today. Nothing is rewritten by the save: the edit enqueues the
/// report-only `event_audit`, which files a `stale_output` finding per disagreement, and repair
/// stays the scoped `event_recompute` a person asks for.
pub struct ConstantOperations;

async fn stored_value(db: &DatabaseConnection, id: Uuid) -> Result<Option<f64>, ApiError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT value FROM constants WHERE id = $1",
            [id.into()],
        ))
        .await
        .map_err(|e| ApiError::from(e))?;
    Ok(row.and_then(|r| r.try_get::<f64>("", "value").ok()))
}

#[must_use]
pub fn audit_dedupe_key(name: &str) -> String {
    format!("event_audit:constant:{name}")
}

#[async_trait]
impl CRUDOperations for ConstantOperations {
    type Resource = Constant;

    /// One row at a time, through the single-row path: the crudcrate default `update_many`
    /// delegates to the resource, which delegates back here, so the default recurses and a bulk
    /// edit would run none of the hooks below.
    async fn update_many(
        &self,
        db: &DatabaseConnection,
        updates: Vec<(Uuid, <Constant as CRUDResource>::UpdateModel)>,
    ) -> Result<Vec<Constant>, ApiError> {
        let mut updated = Vec::with_capacity(updates.len());
        for (id, data) in updates {
            updated.push(self.update(db, id, data).await?);
        }
        Ok(updated)
    }

    async fn create_many(
        &self,
        db: &DatabaseConnection,
        data: Vec<<Constant as CRUDResource>::CreateModel>,
    ) -> Result<Vec<Constant>, ApiError> {
        let mut created = Vec::with_capacity(data.len());
        for item in data {
            created.push(self.create(db, item).await?);
        }
        Ok(created)
    }

    /// The lifecycle with the previous value read first: a units or description edit changes no
    /// calculation and audits nothing.
    async fn update(
        &self,
        db: &DatabaseConnection,
        id: Uuid,
        data: <Constant as CRUDResource>::UpdateModel,
    ) -> Result<Constant, ApiError> {
        let previous = stored_value(db, id).await?;
        self.before_update(db, id, &data).await?;
        let mut entity = self.perform_update(db, id, data).await?;
        self.after_update(db, &mut entity).await?;

        if previous != Some(entity.value) {
            let key = audit_dedupe_key(&entity.name);
            if let Err(e) = worker::enqueue(
                db,
                "event_audit",
                None,
                Some(entity.id),
                &serde_json::json!({ "constant": entity.name }),
                Some(&key),
            )
            .await
            {
                tracing::warn!(error = %e, constant = %entity.name, "constants: failed to enqueue audit");
            }
        }
        Ok(entity)
    }
}
