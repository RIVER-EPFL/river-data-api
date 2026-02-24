use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "device_status")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub parameter_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    pub time: DateTimeWithTimeZone,
    pub battery_level: Option<i16>,
    pub battery_state: Option<i16>,
    pub signal_quality: Option<i16>,
    pub device_status: Option<String>,
    pub unreachable: Option<bool>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::parameters::Entity",
        from = "Column::ParameterId",
        to = "super::parameters::Column::Id"
    )]
    Parameter,
}

impl Related<super::parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Parameter.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
