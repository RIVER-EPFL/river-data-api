use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::DatabaseConnection;
use uuid::Uuid;

use super::model::ReprocessingJob;
use super::registry;

/// The rerun and cancel policies live in [`super::registry`] and are enforced there; a row carries
/// what they say about it so a client renders the two buttons from the server's answer rather than
/// from a copy of the lists.
pub struct ReprocessingJobOperations;

#[async_trait]
impl CRUDOperations for ReprocessingJobOperations {
    type Resource = ReprocessingJob;

    /// One row at a time, through the single-row path: the crudcrate default delegates to the
    /// resource, which delegates back here, and the single-row hooks are what a batch needs too.
    async fn create_many(
        &self,
        db: &DatabaseConnection,
        data: Vec<<ReprocessingJob as CRUDResource>::CreateModel>,
    ) -> Result<Vec<ReprocessingJob>, ApiError> {
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
        updates: Vec<(Uuid, <ReprocessingJob as CRUDResource>::UpdateModel)>,
    ) -> Result<Vec<ReprocessingJob>, ApiError> {
        let mut updated = Vec::with_capacity(updates.len());
        for (id, data) in updates {
            updated.push(self.update(db, id, data).await?);
        }
        Ok(updated)
    }

    async fn after_get_one(
        &self,
        _db: &DatabaseConnection,
        entity: &mut ReprocessingJob,
    ) -> Result<(), ApiError> {
        entity.rerunnable = registry::is_rerunnable(&entity.trigger_type);
        entity.cancellable = registry::is_cancellable(&entity.trigger_type);
        Ok(())
    }

    async fn after_get_all(
        &self,
        _db: &DatabaseConnection,
        entities: &mut Vec<<ReprocessingJob as CRUDResource>::ListModel>,
    ) -> Result<(), ApiError> {
        for entity in entities {
            entity.rerunnable = registry::is_rerunnable(&entity.trigger_type);
            entity.cancellable = registry::is_cancellable(&entity.trigger_type);
        }
        Ok(())
    }
}
