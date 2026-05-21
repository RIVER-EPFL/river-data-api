use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "status_events")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub stream_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub time: DateTimeWithTimeZone,
    pub site_id: Option<Uuid>,
    pub parameter_id: Option<Uuid>,
    pub value: String,
    pub sensor_id: Option<Uuid>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::entity::data_streams::Entity",
        from = "Column::StreamId",
        to = "crate::entity::data_streams::Column::Id"
    )]
    DataStream,
    #[sea_orm(
        belongs_to = "crate::entity::sites::Entity",
        from = "Column::SiteId",
        to = "crate::entity::sites::Column::Id"
    )]
    Site,
    #[sea_orm(
        belongs_to = "crate::entity::parameters::Entity",
        from = "Column::ParameterId",
        to = "crate::entity::parameters::Column::Id"
    )]
    Parameter,
    #[sea_orm(
        belongs_to = "crate::entity::sensors::Entity",
        from = "Column::SensorId",
        to = "crate::entity::sensors::Column::Id"
    )]
    Sensor,
}

impl Related<crate::entity::data_streams::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DataStream.def()
    }
}

impl Related<crate::entity::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Site.def()
    }
}

impl Related<crate::entity::parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Parameter.def()
    }
}

impl Related<crate::entity::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensor.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
