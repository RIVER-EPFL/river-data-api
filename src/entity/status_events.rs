use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "status_events")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub site_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub parameter_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub time: DateTimeWithTimeZone,
    pub value: String,
    pub sensor_id: Option<Uuid>,
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

impl ActiveModelBehavior for ActiveModel {}
