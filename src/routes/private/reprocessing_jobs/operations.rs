use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, TransactionTrait};

use super::model::ReprocessingJob;
use super::registry;

/// The rerun and cancel policies live in [`super::registry`] and are enforced there; a row carries
/// what they say about it so a client renders the two buttons from the server's answer rather than
/// from a copy of the lists.
pub struct ReprocessingJobOperations;

impl CRUDOperations for ReprocessingJobOperations {
    type Resource = ReprocessingJob;

    async fn after_get_one<C: ConnectionTrait + TransactionTrait>(
        &self,
        _db: &C,
        entity: &mut ReprocessingJob,
    ) -> Result<(), ApiError> {
        entity.rerunnable = registry::is_rerunnable(&entity.trigger_type);
        entity.cancellable = registry::is_cancellable(&entity.trigger_type);
        Ok(())
    }

    async fn after_get_all<C: ConnectionTrait + TransactionTrait>(
        &self,
        _db: &C,
        entities: &mut Vec<<ReprocessingJob as CRUDResource>::ListModel>,
    ) -> Result<(), ApiError> {
        for entity in entities {
            entity.rerunnable = registry::is_rerunnable(&entity.trigger_type);
            entity.cancellable = registry::is_cancellable(&entity.trigger_type);
        }
        Ok(())
    }
}
