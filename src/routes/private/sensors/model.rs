use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

use super::operations::SensorOperations;

#[derive(
    Clone,
    Debug,
    DeriveEntityModel,
    serde::Serialize,
    serde::Deserialize,
    EntityToModels,
)]
#[sea_orm(table_name = "sensors")]
#[crudcrate(
    api_struct = "Sensor",
    name_singular = "sensor",
    name_plural = "sensors",
    generate_router,
    operations = SensorOperations
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable, fulltext, sortable)]
    pub serial_number: Option<String>,
    #[crudcrate(fulltext, sortable)]
    pub name: Option<String>,
    #[crudcrate(filterable)]
    pub manufacturer: Option<String>,
    #[crudcrate(filterable)]
    pub model: Option<String>,
    #[crudcrate(filterable)]
    pub is_active: Option<bool>,
    #[crudcrate(filterable)]
    pub is_lab_instrument: Option<bool>,
    pub notes: Option<String>,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub metadata: Option<serde_json::Value>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update), join(one, depth = 1))]
    pub deployments: Vec<crate::routes::private::sensor_deployments::SensorDeployment>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update, list))]
    pub reading_count: Option<i64>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub last_reading_at: Option<chrono::DateTime<chrono::Utc>>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub last_calibration_at: Option<chrono::DateTime<chrono::Utc>>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub current_site_id: Option<Uuid>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub current_site_name: Option<String>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub last_reading_value: Option<f64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "crate::routes::private::sensor_calibrations::Entity")]
    SensorCalibrations,
    #[sea_orm(has_many = "crate::routes::private::sensor_deployments::Entity")]
    SensorDeployments,
    #[sea_orm(has_many = "crate::routes::private::readings::Entity")]
    Readings,
}

impl Related<crate::routes::private::sensor_calibrations::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SensorCalibrations.def()
    }
}

impl Related<crate::routes::private::sensor_deployments::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SensorDeployments.def()
    }
}

impl Related<crate::routes::private::readings::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Readings.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
