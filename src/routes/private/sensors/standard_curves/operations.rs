use crudcrate::{ApiError, CRUDOperations};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter, TransactionTrait,
};
use uuid::Uuid;

use super::model::{Column, Entity, StandardCurve};
use crate::routes::private::annotations::models as annotations;
use crate::routes::private::readings::models as readings;

pub struct StandardCurveOperations;

/// Whether any reading was corrected with this curve. A curve in use is frozen: the value stored on
/// the reading was computed from these coefficients, so editing them in place would silently rewrite
/// published values with no record that it happened.
///
/// This is deliberately unlike a windowed `sensor_calibration`, where an edit is expected to
/// reprocess the readings its window covers. A standard curve is picked by hand for one measurement,
/// so there is no window to reprocess and no way to tell which readings the operator meant to change.
/// A corrected curve is a new row, and the affected grabs are re-entered against it.
async fn curve_is_used<C: ConnectionTrait>(db: &C, id: Uuid) -> Result<bool, ApiError> {
    let on_a_reading = readings::Entity::find()
        .filter(readings::Column::StandardCurveId.eq(id))
        .one(db)
        .await
        .map_err(ApiError::database)?
        .is_some();
    if on_a_reading {
        return Ok(true);
    }
    let on_an_annotation = annotations::Entity::find()
        .filter(annotations::Column::StandardCurveId.eq(id))
        .one(db)
        .await
        .map_err(ApiError::database)?
        .is_some();
    Ok(on_an_annotation)
}

/// How many curves were copied from this one. A copy records where its coefficients came from, and
/// the reference is the only statement that it is a copy at all, so the row it names stays.
async fn copies_made_from<C: ConnectionTrait>(db: &C, id: Uuid) -> Result<u64, ApiError> {
    Entity::find()
        .filter(Column::CopiedFromId.eq(id))
        .count(db)
        .await
        .map_err(ApiError::database)
}

impl CRUDOperations for StandardCurveOperations {
    type Resource = StandardCurve;

    async fn before_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        _db: &C,
        data: &<StandardCurve as crudcrate::CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        if data.slope == 0.0 {
            return Err(ApiError::bad_request(
                "Slope cannot be zero: all readings would produce a constant value".to_string(),
            ));
        }
        Ok(())
    }

    async fn before_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
        data: &<StandardCurve as crudcrate::CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        if data.slope == Some(Some(0.0)) {
            return Err(ApiError::bad_request(
                "Slope cannot be zero: all readings would produce a constant value".to_string(),
            ));
        }

        // Everything except `notes` is the provenance of a value that has already been published,
        // `r_squared` and `created_by` included: they record how and by whom the fit that produced
        // that value was obtained. Only free text may still be added after the fact.
        let frozen_field_change = data.slope.is_some()
            || data.intercept.is_some()
            || data.r_squared.is_some()
            || data.name.is_some()
            || data.fitted_on.is_some()
            || data.sensor_id.is_some()
            || data.created_by.is_some();
        if frozen_field_change && curve_is_used(db, id).await? {
            return Err(ApiError::bad_request(
                "This standard curve has already been applied to readings, so its coefficients, \
                 fit quality, name, fit date, instrument and attribution are fixed. Create a new \
                 curve and re-enter the affected measurements against it. Only its notes stay \
                 editable."
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Restrict, never cascade: the readings keep their reference and the delete is refused. The
    /// foreign key already refuses it, but reports a constraint violation the CRUD layer surfaces as
    /// an internal error, so the check here is what makes it a stated 400 while the constraint stays
    /// the backstop for raw SQL.
    async fn before_delete<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
    ) -> Result<(), ApiError> {
        if curve_is_used(db, id).await? {
            return Err(ApiError::bad_request(format!(
                "Standard curve {id} has been applied to readings and cannot be deleted: the \
                 readings would lose the curve that produced their values."
            )));
        }
        let copies = copies_made_from(db, id).await?;
        if copies > 0 {
            return Err(ApiError::bad_request(format!(
                "Standard curve {id} was copied onto {copies} other instrument{} and cannot be \
                 deleted: each copy names it as where its coefficients came from.",
                if copies == 1 { "" } else { "s" }
            )));
        }
        Ok(())
    }
}
