use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "readings")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub site_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub parameter_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub time: DateTimeWithTimeZone,
    pub raw_value: f64,
    pub calibrated_value: Option<f64>,
    pub sensor_id: Option<Uuid>,
    pub calibration_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
    pub logged: Option<bool>,
    pub measurement_type: Option<String>,
    pub is_flagged: Option<bool>,
    pub flag_reason: Option<String>,
    pub field_trip_id: Option<Uuid>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::sites::Entity",
        from = "Column::SiteId",
        to = "super::sites::Column::Id"
    )]
    Site,
    #[sea_orm(
        belongs_to = "super::parameters::Entity",
        from = "Column::ParameterId",
        to = "super::parameters::Column::Id"
    )]
    Parameter,
    #[sea_orm(
        belongs_to = "super::sensors::Entity",
        from = "Column::SensorId",
        to = "super::sensors::Column::Id"
    )]
    Sensor,
    #[sea_orm(
        belongs_to = "super::sensor_calibrations::Entity",
        from = "Column::CalibrationId",
        to = "super::sensor_calibrations::Column::Id"
    )]
    SensorCalibration,
    #[sea_orm(
        belongs_to = "super::sensor_deployments::Entity",
        from = "Column::DeploymentId",
        to = "super::sensor_deployments::Column::Id"
    )]
    SensorDeployment,
    #[sea_orm(
        belongs_to = "super::field_trips::Entity",
        from = "Column::FieldTripId",
        to = "super::field_trips::Column::Id"
    )]
    FieldTrip,
}

impl Related<super::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Site.def()
    }
}

impl Related<super::parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Parameter.def()
    }
}

impl Related<super::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensor.def()
    }
}

impl Related<super::sensor_calibrations::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SensorCalibration.def()
    }
}

impl Related<super::sensor_deployments::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SensorDeployment.def()
    }
}

impl Related<super::field_trips::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::FieldTrip.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
