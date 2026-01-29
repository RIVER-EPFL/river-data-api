use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "alarm_locations")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub alarm_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub sensor_id: Uuid,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::alarms::Entity",
        from = "Column::AlarmId",
        to = "super::alarms::Column::Id"
    )]
    Alarm,
    #[sea_orm(
        belongs_to = "super::sensors::Entity",
        from = "Column::SensorId",
        to = "super::sensors::Column::Id"
    )]
    Sensor,
}

impl Related<super::alarms::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Alarm.def()
    }
}

impl Related<super::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensor.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
