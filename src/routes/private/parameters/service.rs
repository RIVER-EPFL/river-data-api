use crudcrate::{ApiError, CRUDOperations};
use sea_orm::{ConnectionTrait, TransactionTrait};
use uuid::Uuid;

use super::models::Parameter;
use crate::routes::private::alarms::flows::reconcile_all_from_hook;

/// A parameter's own bounds are its global `alarm_thresholds` row, so nothing on the parameter
/// itself is read by alarm evaluation and an edit cannot change breach state. Removing the
/// parameter can: its slots and its thresholds go with it, so the delete hooks reconcile
/// immediately instead of waiting for the backstop sweep.
pub struct ParameterOperations;

impl CRUDOperations for ParameterOperations {
    type Resource = Parameter;

    /// The change-audit trigger reads the writer from the transaction, so the label is declared on
    /// every write this entity makes, before any hook or statement on it (B185).
    async fn after_begin<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
    ) -> Result<(), ApiError> {
        crate::common::actor::declare(db)
            .await
            .map_err(ApiError::database)
    }

    async fn after_delete<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        _id: Uuid,
    ) -> Result<(), ApiError> {
        reconcile_all_from_hook(db).await;
        Ok(())
    }
}
