use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;

use super::service::SensorCalibrationOperations;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "sensor_calibrations")]
#[crudcrate(
    api_struct = "SensorCalibration",
    name_singular = "sensor_calibration",
    name_plural = "sensor_calibrations",
    generate_router,
    operations = SensorCalibrationOperations
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub sensor_id: Uuid,
    pub slope: f64,
    pub intercept: f64,
    #[crudcrate(sortable)]
    pub valid_from: chrono::DateTime<chrono::Utc>,
    pub performed_by: Option<String>,
    pub notes: Option<String>,
    /// Human label for the curve, an alternative to picking it by date in the editor.
    pub name: Option<String>,
    /// Per-channel parameter (multi-parameter instruments get one curve per channel); NULL applies
    /// the curve to every channel.
    #[crudcrate(filterable)]
    pub parameter_id: Option<Uuid>,
    pub r_squared: Option<f64>,
    /// End of the window, exclusive. Normally chain-written (the next curve's `valid_from`), but an
    /// operator may retire a curve by setting it on update. A retired curve leaves the time after
    /// it uncovered: readings there keep the calibration they were stamped with, because reprocess
    /// only rewrites a reading a curve covers.
    #[crudcrate(exclude(create))]
    pub valid_until: Option<chrono::DateTime<chrono::Utc>>,
    /// True when `valid_until` was set by an operator rather than by the window chain. Provenance,
    /// not data: no client sets it, `before_update` maintains it from what the update carried.
    #[crudcrate(exclude(create, update), on_create = false)]
    pub valid_until_explicit: bool,
    #[crudcrate(exclude(create, update))]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When the curve was taken out of circulation (M146). A retired curve is never resolved for a
    /// reading again and never bounds another curve's window; the row, its coefficients and the
    /// readings it corrected stay. Written by the retire routes, never through CRUD.
    #[crudcrate(exclude(create, update), filterable, sortable)]
    pub retired_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update))]
    pub retired_by: Option<String>,
    #[crudcrate(exclude(create, update))]
    pub retired_reason: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::sensors::Entity",
        from = "Column::SensorId",
        to = "crate::routes::private::sensors::Column::Id"
    )]
    Sensor,
    #[sea_orm(has_many = "crate::routes::private::readings::Entity")]
    Readings,
}

impl Related<crate::routes::private::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensor.def()
    }
}

impl Related<crate::routes::private::readings::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Readings.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

// Field validation lives on the request models, where crudcrate already runs it: the generated
// handlers call `Validatable::validate` on the create and update models before any hook, so one
// statement of a rule covers both routes and both batch paths. A rule that has to read the
// database (an overlapping window, a duplicate instant) stays a hook, because this sees only the
// request.

impl crudcrate::validation::Validatable for SensorCalibrationCreate {
    fn validate(&self) -> Result<(), crudcrate::validation::ValidationError> {
        slope_is_usable(Some(self.slope))
    }
}

impl crudcrate::validation::Validatable for SensorCalibrationUpdate {
    fn validate(&self) -> Result<(), crudcrate::validation::ValidationError> {
        // An update model carries `Option<Option<T>>`: absent, explicitly null, or a value.
        slope_is_usable(self.slope.flatten())
    }
}

/// A zero slope maps every raw value onto the intercept, so the instrument's readings become one
/// constant and no reprocess can recover them.
fn slope_is_usable(slope: Option<f64>) -> Result<(), crudcrate::validation::ValidationError> {
    if slope == Some(0.0) {
        return Err(crudcrate::validation::ValidationError::new(
            "slope",
            "Slope cannot be zero: all readings would produce a constant value",
        ));
    }
    Ok(())
}
