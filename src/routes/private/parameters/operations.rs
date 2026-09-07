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

    async fn after_delete(&self, db: &DatabaseConnection, _id: Uuid) -> Result<(), ApiError> {
        reconcile_all_from_hook(db).await;
        Ok(())
    }
}
