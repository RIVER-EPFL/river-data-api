use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations};
use sea_orm::DatabaseConnection;
use uuid::Uuid;

use super::model::Parameter;
use crate::routes::private::alarms::sweeper::reconcile_all_from_hook;

/// Alarm thresholds are intentionally NOT auto-created from a parameter's `default_*` columns when
/// the defaults change. Alarm evaluation already falls back to those defaults (priority 3) whenever
/// no `alarm_thresholds` row exists, so materializing default-valued rows is redundant — and a
/// site-specific copy would silently shadow a global threshold an operator set. A threshold row
/// exists only when a user explicitly creates one via the editor.
///
/// Because evaluation reads `default_*` live, an edit can change breach state with no new reading;
/// the post-mutation hooks reconcile immediately instead of waiting for the backstop sweep. The
/// hook can't see the previous values to detect whether defaults actually changed, so it runs
/// unconditionally — parameter edits are rare and the reconcile is O(active slots).
pub struct ParameterOperations;

#[async_trait]
impl CRUDOperations for ParameterOperations {
    type Resource = Parameter;

    async fn after_update(
        &self,
        db: &DatabaseConnection,
        _entity: &mut Parameter,
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
