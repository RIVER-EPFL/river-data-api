use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    DeriveEntityModel,
    Serialize,
    Deserialize,
    EntityToModels,
)]
#[sea_orm(table_name = "data_streams")]
#[crudcrate(
    api_struct = "DataStream",
    name_singular = "data_stream",
    name_plural = "data_streams",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub source_system: String,
    #[crudcrate(filterable)]
    pub source_key: String,
    pub source_name: Option<String>,
    pub source_path: Option<String>,
    #[sea_orm(column_type = "JsonBinary")]
    pub metadata: serde_json::Value,
    #[crudcrate(filterable)]
    pub site_parameter_id: Option<Uuid>,
    #[crudcrate(filterable)]
    pub sensor_id: Option<Uuid>,
    #[crudcrate(filterable)]
    pub is_active: bool,
    #[crudcrate(sortable, exclude(create, update))]
    pub discovered_at: DateTimeWithTimeZone,
    #[crudcrate(sortable)]
    pub paired_at: Option<DateTimeWithTimeZone>,
    #[crudcrate(sortable)]
    pub last_data_time: Option<DateTimeWithTimeZone>,
    #[crudcrate(exclude(create, update))]
    pub created_at: DateTimeWithTimeZone,
    #[crudcrate(exclude(create, update))]
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::site_parameters::Entity",
        from = "Column::SiteParameterId",
        to = "super::site_parameters::Column::Id"
    )]
    SiteParameter,
    #[sea_orm(
        belongs_to = "super::sensors::Entity",
        from = "Column::SensorId",
        to = "super::sensors::Column::Id"
    )]
    Sensor,
}

impl Related<super::site_parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SiteParameter.def()
    }
}

impl Related<super::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensor.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
