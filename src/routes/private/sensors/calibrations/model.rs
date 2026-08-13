use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

use super::operations::SensorCalibrationOperations;

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
