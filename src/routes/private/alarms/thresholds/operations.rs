use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations};
use sea_orm::DatabaseConnection;
use uuid::Uuid;

use super::model::AlarmThreshold;
use crate::routes::private::alarms::sweeper::reconcile_all_from_hook;

/// Threshold changes re-check breach state immediately instead of waiting for the periodic
/// backstop sweep. The reconcile is global (all active slots): threshold edits are rare, the
/// post-LATERAL-rewrite sweep is O(active slots), and a global threshold (`site_id IS NULL`)
/// fans out across every site carrying the parameter anyway.
pub struct AlarmThresholdOperations;

#[async_trait]
impl CRUDOperations for AlarmThresholdOperations {
    type Resource = AlarmThreshold;

    /// One row at a time, through the single-row path: the crudcrate default `create_many`
    /// delegates to the resource, which delegates back here, so the default recurses; the loop
    /// also runs the single-row hooks for every item.
    async fn create_many(
        &self,
        db: &DatabaseConnection,
        data: Vec<<AlarmThreshold as crudcrate::CRUDResource>::CreateModel>,
    ) -> Result<Vec<AlarmThreshold>, ApiError> {
        let mut created = Vec::with_capacity(data.len());
        for item in data {
            created.push(self.create(db, item).await?);
        }
        Ok(created)
    }

    async fn after_create(
        &self,
        db: &DatabaseConnection,
        _entity: &mut AlarmThreshold,
    ) -> Result<(), ApiError> {
        reconcile_all_from_hook(db).await;
        Ok(())
    }

    async fn after_update(
        &self,
        db: &DatabaseConnection,
        _entity: &mut AlarmThreshold,
    ) -> Result<(), ApiError> {
        reconcile_all_from_hook(db).await;
        Ok(())
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
