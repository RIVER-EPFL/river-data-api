use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

use super::operations::StandardCurveOperations;

/// A lab curve applied on top of an instrument's base calibration, chosen by hand per measurement
/// (typically per microplate) rather than resolved by time. It belongs to one instrument and
/// carries no time columns at all: nothing here is ever selected by a window, which is what keeps
/// it out of the calibration chaining and reprocessing machinery.
#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "standard_curves")]
#[crudcrate(
    api_struct = "StandardCurve",
    name_singular = "standard_curve",
    name_plural = "standard_curves",
    generate_router,
    operations = StandardCurveOperations
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub sensor_id: Uuid,
    /// Human label the operator picks the curve by, for example the plate it was fitted from.
    #[crudcrate(filterable, fulltext, sortable)]
    pub name: Option<String>,
    pub slope: f64,
    pub intercept: f64,
    /// Fit quality reported by whatever produced the curve; recorded, never used in arithmetic.
    pub r_squared: Option<f64>,
    pub notes: Option<String>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Who fitted the curve, supplied by the caller as on notes, annotations, samples and pairing
    /// plans. Writable, otherwise the column could never hold anything.
    pub created_by: Option<String>,
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
