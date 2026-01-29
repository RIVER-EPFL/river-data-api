use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "events")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub time: DateTimeWithTimeZone,
    #[sea_orm(primary_key, auto_increment = false)]
    pub vaisala_event_num: i32,
    pub category: String,
    pub message: String,
    pub user_name: Option<String>,
    pub entity: Option<String>,
    pub entity_id: Option<i32>,
    pub sensor_id: Option<Uuid>,
    pub station_id: Option<Uuid>,
    pub device_id: Option<i32>,
    pub channel_id: Option<i32>,
    pub host_id: Option<i32>,
    #[sea_orm(column_type = "JsonBinary")]
    pub extra_fields: Option<serde_json::Value>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::sensors::Entity",
        from = "Column::SensorId",
        to = "super::sensors::Column::Id"
    )]
    Sensor,
    #[sea_orm(
        belongs_to = "super::stations::Entity",
        from = "Column::StationId",
        to = "super::stations::Column::Id"
    )]
    Station,
}

impl Related<super::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensor.def()
    }
}

impl Related<super::stations::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Station.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
