use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "alarms")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    #[sea_orm(unique)]
    pub vaisala_alarm_id: i32,
    pub severity: i16,
    pub description: String,
    pub error_text: Option<String>,
    pub alarm_type: Option<String>,
    pub when_on: DateTimeWithTimeZone,
    pub when_off: Option<DateTimeWithTimeZone>,
    pub when_ack: Option<DateTimeWithTimeZone>,
    pub when_condition: Option<DateTimeWithTimeZone>,
    pub duration_sec: Option<f64>,
    pub status: bool,
    pub is_system: bool,
    pub serial_number: Option<String>,
    pub location_text: Option<String>,
    pub zone_text: Option<String>,
    pub station_id: Option<Uuid>,
    pub ack_required: bool,
    #[sea_orm(column_type = "JsonBinary")]
    pub ack_comments: Option<serde_json::Value>,
    pub ack_action_taken: Option<String>,
    pub created_at: Option<DateTimeWithTimeZone>,
    pub updated_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::alarm_locations::Entity")]
    AlarmLocations,
    #[sea_orm(
        belongs_to = "super::stations::Entity",
        from = "Column::StationId",
        to = "super::stations::Column::Id"
    )]
    Station,
}

impl Related<super::alarm_locations::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::AlarmLocations.def()
    }
}

impl Related<super::stations::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Station.def()
    }
}

impl Related<super::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        super::alarm_locations::Relation::Sensor.def()
    }

    fn via() -> Option<RelationDef> {
        Some(super::alarm_locations::Relation::Alarm.def().rev())
    }
}

impl ActiveModelBehavior for ActiveModel {}
