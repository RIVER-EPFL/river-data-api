use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations};
use sea_orm::DatabaseConnection;
use uuid::Uuid;

use super::model::Parameter;
use crate::routes::private::alarms::sweeper::reconcile_all_from_hook;

/// A parameter's own bounds are its global `alarm_thresholds` row, so nothing on the parameter
/// itself is read by alarm evaluation and an edit cannot change breach state. Removing the
/// parameter can: its slots and its thresholds go with it, so the delete hooks reconcile
/// immediately instead of waiting for the backstop sweep.
pub struct ParameterOperations;

#[async_trait]
impl CRUDOperations for ParameterOperations {
    type Resource = Parameter;

    /// One row at a time, through the single-row path: the crudcrate default delegates to the
    /// resource, which delegates back here, and the single-row hooks are what a batch needs too.
    async fn update_many(
        &self,
        db: &DatabaseConnection,
        updates: Vec<(Uuid, <Parameter as crudcrate::CRUDResource>::UpdateModel)>,
    ) -> Result<Vec<Parameter>, ApiError> {
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
        data: Vec<<Parameter as crudcrate::CRUDResource>::CreateModel>,
    ) -> Result<Vec<Parameter>, ApiError> {
        let mut created = Vec::with_capacity(data.len());
        for item in data {
            created.push(self.create(db, item).await?);
        }
        Ok(created)
    }

    async fn after_delete(&self, db: &DatabaseConnection, _id: Uuid) -> Result<(), ApiError> {
        reconcile_all_from_hook(db).await;
        Ok(())
    }

    async fn after_delete_many(
        &self,
        db: &DatabaseConnection,
        _ids: &[Uuid],
    ) -> Result<(), ApiError> {
        reconcile_all_from_hook(db).await;
        Ok(())
    }
}
