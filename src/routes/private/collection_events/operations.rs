use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use super::model::CollectionEvent;

pub struct CollectionEventOperations;

/// How many readings the event holds. The FK is `ON DELETE SET NULL`, so a delete would leave
/// them attached to no visit with no route to re-attach them.
async fn attached_readings(db: &DatabaseConnection, id: Uuid) -> Result<i64, ApiError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COUNT(*) AS n FROM readings WHERE collection_event_id = $1",
            [id.into()],
        ))
        .await
        .map_err(ApiError::database)?;
    Ok(row
        .map(|r| r.try_get::<i64>("", "n").unwrap_or(0))
        .unwrap_or(0))
}

#[async_trait]
impl CRUDOperations for CollectionEventOperations {
    type Resource = CollectionEvent;

    /// One row at a time, through the single-row path: the crudcrate default delegates to the
    /// resource, which delegates back here, and the single-row hooks are what a batch needs too.
    async fn create_many(
        &self,
        db: &DatabaseConnection,
        data: Vec<<CollectionEvent as crudcrate::CRUDResource>::CreateModel>,
    ) -> Result<Vec<CollectionEvent>, ApiError> {
        let mut created = Vec::with_capacity(data.len());
        for item in data {
            created.push(self.create(db, item).await?);
        }
        Ok(created)
    }

    /// One row at a time, through the single-row path: the crudcrate default delegates to the
    /// resource, which delegates back here, and the single-row hooks are what a batch needs too.
    async fn update_many(
        &self,
        db: &DatabaseConnection,
        updates: Vec<(Uuid, <CollectionEvent as crudcrate::CRUDResource>::UpdateModel)>,
    ) -> Result<Vec<CollectionEvent>, ApiError> {
        let mut updated = Vec::with_capacity(updates.len());
        for (id, data) in updates {
            updated.push(self.update(db, id, data).await?);
        }
        Ok(updated)
    }

    async fn before_delete(&self, db: &DatabaseConnection, id: Uuid) -> Result<(), ApiError> {
        let n = attached_readings(db, id).await?;
        if n > 0 {
            return Err(ApiError::conflict(format!(
                "Collection event {id} holds {n} reading{} and cannot be deleted: the readings \
                 would be detached from every visit.",
                if n == 1 { "" } else { "s" }
            )));
        }
        Ok(())
    }

    async fn before_delete_many(
        &self,
        db: &DatabaseConnection,
        ids: &[Uuid],
    ) -> Result<(), ApiError> {
        for id in ids {
            self.before_delete(db, *id).await?;
        }
        Ok(())
    }
}
