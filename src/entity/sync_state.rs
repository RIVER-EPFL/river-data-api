use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "sync_state")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub sensor_id: Uuid,
    pub last_data_time: Option<DateTimeWithTimeZone>,
    pub last_sync_attempt: Option<DateTimeWithTimeZone>,
    pub sync_status: Option<String>,
    pub error_message: Option<String>,
    pub retry_count: Option<i32>,
    pub last_full_sync: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::sensors::Entity",
        from = "Column::SensorId",
        to = "super::sensors::Column::Id"
    )]
    Sensor,
}

impl Related<super::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensor.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
