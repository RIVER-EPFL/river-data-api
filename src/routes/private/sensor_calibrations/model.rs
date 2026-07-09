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
    /// Human label for picking an instant (lab grab) curve by property rather than by date.
    pub name: Option<String>,
    /// `windowed` (field sensor, auto-applied over a time window) or `instant` (lab grab,
    /// applied to a single reading). DB-defaulted to `windowed`; the grab path sets `instant`.
    #[crudcrate(exclude(create, update))]
    pub mode: String,
    /// Per-channel parameter for windowed field curves (multi-parameter instruments get one curve
    /// per channel); NULL for lab instant curves, whose parameter is decided at the grab.
    #[crudcrate(filterable)]
    pub parameter_id: Option<Uuid>,
    pub r_squared: Option<f64>,
    #[crudcrate(exclude(create, update))]
    pub valid_until: Option<chrono::DateTime<chrono::Utc>>,
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
