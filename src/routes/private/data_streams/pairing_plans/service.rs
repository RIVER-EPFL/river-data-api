//! The hook the pairing plan CRUD routes run.

use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, TransactionTrait};
use uuid::Uuid;

use super::models::PairingPlan;

pub struct PairingPlanOperations;

impl CRUDOperations for PairingPlanOperations {
    type Resource = PairingPlan;

    async fn before_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        _db: &C,
        _id: Uuid,
        data: &<PairingPlan as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        crate::common::actor::refuse_reattribution(data.created_by.is_some())
    }
}
