use crudcrate::{ApiError, CRUDOperations};
use sea_orm::{ConnectionTrait, TransactionTrait};
use uuid::Uuid;

use super::model::AlarmThreshold;
use crate::routes::private::alarms::sweeper::reconcile_all_from_hook;

/// Threshold changes re-check breach state immediately instead of waiting for the periodic
/// backstop sweep. The reconcile is global (all active slots): threshold edits are rare, the
/// post-LATERAL-rewrite sweep is O(active slots), and a global threshold (`site_id IS NULL`)
/// fans out across every site carrying the parameter anyway.
pub struct AlarmThresholdOperations;

impl CRUDOperations for AlarmThresholdOperations {
    type Resource = AlarmThreshold;

    async fn after_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        _entity: &mut AlarmThreshold,
    ) -> Result<(), ApiError> {
        reconcile_all_from_hook(db).await;
        Ok(())
    }

    async fn after_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        _entity: &mut AlarmThreshold,
    ) -> Result<(), ApiError> {
        reconcile_all_from_hook(db).await;
        Ok(())
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
