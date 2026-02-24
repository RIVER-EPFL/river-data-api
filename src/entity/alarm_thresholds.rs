use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "alarm_thresholds")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub parameter_id: Uuid,
    /// Value below this triggers warning (severity 1)
    pub warning_min: Option<f64>,
    /// Value above this triggers warning (severity 1)
    pub warning_max: Option<f64>,
    /// Value below this triggers alarm (severity 2)
    pub alarm_min: Option<f64>,
    /// Value above this triggers alarm (severity 2)
    pub alarm_max: Option<f64>,
    pub description: Option<String>,
    pub created_at: Option<DateTimeWithTimeZone>,
    pub updated_at: Option<DateTimeWithTimeZone>,
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
