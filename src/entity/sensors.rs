use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "sensors")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub station_id: Uuid,
    #[sea_orm(unique)]
    pub vaisala_location_id: i32,
    pub name: String,
    pub sensor_type: String,
    pub display_units: Option<String>,
    pub units_name: Option<String>,
    pub units_min: Option<f64>,
    pub units_max: Option<f64>,
    pub decimal_places: Option<i16>,
    pub device_serial_number: Option<String>,
    pub probe_serial_number: Option<String>,
    pub channel_id: Option<i32>,
    pub sample_interval_sec: Option<i32>,
    pub is_active: Option<bool>,
    pub created_at: Option<DateTimeWithTimeZone>,
    pub updated_at: Option<DateTimeWithTimeZone>,
    pub discovered_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::stations::Entity",
        from = "Column::StationId",
        to = "super::stations::Column::Id"
    )]
    Station,
    #[sea_orm(has_many = "super::readings::Entity")]
    Readings,
    #[sea_orm(has_many = "super::device_status::Entity")]
    DeviceStatus,
    #[sea_orm(has_many = "super::calibrations::Entity")]
    Calibrations,
    #[sea_orm(has_one = "super::sync_state::Entity")]
    SyncState,
    #[sea_orm(has_many = "super::events::Entity")]
    Events,
    #[sea_orm(has_many = "super::alarm_locations::Entity")]
    AlarmLocations,
}

impl Related<super::stations::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Station.def()
    }
}

impl Related<super::readings::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Readings.def()
    }
}

impl Related<super::device_status::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DeviceStatus.def()
    }
}

impl Related<super::calibrations::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Calibrations.def()
    }
}

impl Related<super::sync_state::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncState.def()
    }
}

impl Related<super::events::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Events.def()
    }
}

impl Related<super::alarm_locations::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::AlarmLocations.def()
    }
}

impl Related<super::alarms::Entity> for Entity {
    fn to() -> RelationDef {
        super::alarm_locations::Relation::Alarm.def()
    }

    fn via() -> Option<RelationDef> {
        Some(super::alarm_locations::Relation::Sensor.def().rev())
    }
}

impl ActiveModelBehavior for ActiveModel {}
