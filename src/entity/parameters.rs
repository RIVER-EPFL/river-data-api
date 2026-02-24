use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "parameters")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub site_id: Uuid,
    #[sea_orm(unique)]
    pub source_location_id: i32,
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
        belongs_to = "super::sites::Entity",
        from = "Column::SiteId",
        to = "super::sites::Column::Id"
    )]
    Site,
    #[sea_orm(has_many = "super::readings::Entity")]
    Readings,
    #[sea_orm(has_many = "super::device_status::Entity")]
    DeviceStatus,
    #[sea_orm(has_many = "super::calibrations::Entity")]
    Calibrations,
    #[sea_orm(has_one = "super::sync_state::Entity")]
    SyncState,
    #[sea_orm(has_one = "super::alarm_thresholds::Entity")]
    AlarmThresholds,
}

impl Related<super::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Site.def()
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

impl Related<super::alarm_thresholds::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::AlarmThresholds.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
