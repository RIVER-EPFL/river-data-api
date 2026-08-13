use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "readings")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub stream_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub time: DateTimeWithTimeZone,
    #[sea_orm(primary_key, auto_increment = false)]
    pub replicate_index: i16,
    pub site_id: Option<Uuid>,
    pub parameter_id: Option<Uuid>,
    pub raw_value: f64,
    pub calibrated_value: Option<f64>,
    pub sensor_id: Option<Uuid>,
    /// The time-windowed base calibration the value was corrected with.
    pub calibration_id: Option<Uuid>,
    /// The hand-picked lab curve applied on top of the base calibration, for grab measurements.
    pub standard_curve_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
    pub logged: Option<bool>,
    pub measurement_type: Option<String>,
    pub is_flagged: Option<bool>,
    pub flag_reason: Option<String>,
    pub sample_id: Option<Uuid>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::data_streams::Entity",
        from = "Column::StreamId",
        to = "crate::routes::private::data_streams::Column::Id"
    )]
    DataStream,
    #[sea_orm(
        belongs_to = "crate::routes::private::sites::Entity",
        from = "Column::SiteId",
        to = "crate::routes::private::sites::Column::Id"
    )]
    Site,
    #[sea_orm(
        belongs_to = "crate::routes::private::parameters::Entity",
        from = "Column::ParameterId",
        to = "crate::routes::private::parameters::Column::Id"
    )]
    Parameter,
    #[sea_orm(
        belongs_to = "crate::routes::private::sensors::Entity",
        from = "Column::SensorId",
        to = "crate::routes::private::sensors::Column::Id"
    )]
    Sensor,
    #[sea_orm(
        belongs_to = "crate::routes::private::sensors::calibrations::Entity",
        from = "Column::CalibrationId",
        to = "crate::routes::private::sensors::calibrations::Column::Id"
    )]
    SensorCalibration,
    #[sea_orm(
        belongs_to = "crate::routes::private::sensors::standard_curves::Entity",
        from = "Column::StandardCurveId",
        to = "crate::routes::private::sensors::standard_curves::Column::Id"
    )]
    StandardCurve,
    #[sea_orm(
        belongs_to = "crate::routes::private::sensors::deployments::Entity",
        from = "Column::DeploymentId",
        to = "crate::routes::private::sensors::deployments::Column::Id"
    )]
    SensorDeployment,
    #[sea_orm(
        belongs_to = "crate::routes::private::readings::samples::Entity",
        from = "Column::SampleId",
        to = "crate::routes::private::readings::samples::Column::Id",
        on_delete = "SetNull"
    )]
    Sample,
}

impl Related<crate::routes::private::data_streams::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DataStream.def()
    }
}

impl Related<crate::routes::private::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Site.def()
    }
}

impl Related<crate::routes::private::parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Parameter.def()
    }
}

impl Related<crate::routes::private::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensor.def()
    }
}

impl Related<crate::routes::private::sensors::calibrations::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SensorCalibration.def()
    }
}

impl Related<crate::routes::private::sensors::standard_curves::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::StandardCurve.def()
    }
}

impl Related<crate::routes::private::sensors::deployments::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SensorDeployment.def()
    }
}

impl Related<crate::routes::private::readings::samples::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sample.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
