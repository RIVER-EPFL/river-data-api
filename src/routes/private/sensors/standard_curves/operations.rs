use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use super::model::StandardCurve;

pub struct StandardCurveOperations;

/// Whether any reading was corrected with this curve. A curve in use is frozen: the value stored on
/// the reading was computed from these coefficients, so editing them in place would silently rewrite
/// published values with no record that it happened.
///
/// This is deliberately unlike a windowed `sensor_calibration`, where an edit is expected to
/// reprocess the readings its window covers. A standard curve is picked by hand for one measurement,
/// so there is no window to reprocess and no way to tell which readings the operator meant to change.
/// A corrected curve is a new row, and the affected grabs are re-entered against it.
async fn curve_is_used(db: &DatabaseConnection, id: Uuid) -> Result<bool, ApiError> {
    let found = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT 1 AS one FROM readings WHERE standard_curve_id = $1 LIMIT 1",
            [id.into()],
        ))
        .await
        .map_err(ApiError::database)?;
    Ok(found.is_some())
}

#[async_trait]
impl CRUDOperations for StandardCurveOperations {
    type Resource = StandardCurve;

    async fn before_create(
        &self,
        _db: &DatabaseConnection,
        data: &<StandardCurve as crudcrate::CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        if data.slope == 0.0 {
            return Err(ApiError::bad_request(
                "Slope cannot be zero: all readings would produce a constant value".to_string(),
            ));
        }
        Ok(())
    }

    /// One row at a time, through the single-row path, so a bulk edit still meets the rule that a
    /// curve a reading references is frozen.
    async fn update_many(
        &self,
        db: &DatabaseConnection,
        updates: Vec<(
            Uuid,
            <StandardCurve as crudcrate::CRUDResource>::UpdateModel,
        )>,
    ) -> Result<Vec<StandardCurve>, ApiError> {
        let mut updated = Vec::with_capacity(updates.len());
        for (id, data) in updates {
            updated.push(self.update(db, id, data).await?);
        }
        Ok(updated)
    }

    async fn before_update(
        &self,
        db: &DatabaseConnection,
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
            || data.sensor_id.is_some()
            || data.created_by.is_some();
        if frozen_field_change && curve_is_used(db, id).await? {
            return Err(ApiError::bad_request(
                "This standard curve has already been applied to readings, so its coefficients, \
                 fit quality, name, instrument and attribution are fixed. Create a new curve and \
                 re-enter the affected measurements against it. Only its notes stay editable."
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Restrict, never cascade: the readings keep their reference and the delete is refused. The
    /// foreign key already refuses it, but reports a constraint violation the CRUD layer surfaces as
    /// an internal error, so the check here is what makes it a stated 400 while the constraint stays
    /// the backstop for raw SQL.
    async fn before_delete(&self, db: &DatabaseConnection, id: Uuid) -> Result<(), ApiError> {
        if curve_is_used(db, id).await? {
            return Err(ApiError::bad_request(format!(
                "Standard curve {id} has been applied to readings and cannot be deleted: the \
                 readings would lose the curve that produced their values."
            )));
        }
        Ok(())
    }

    async fn before_delete_many(
        &self,
        db: &DatabaseConnection,
        ids: &[Uuid],
    ) -> Result<(), ApiError> {
        for id in ids {
            self.before_delete(db, *id).await?;
        }
        Ok(())
    }
}
