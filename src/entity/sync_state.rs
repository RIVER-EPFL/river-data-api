use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "sync_state")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub site_parameter_id: Uuid,
    pub last_data_time: Option<DateTimeWithTimeZone>,
    pub last_sync_attempt: Option<DateTimeWithTimeZone>,
    pub sync_status: Option<String>,
    pub error_message: Option<String>,
    pub retry_count: Option<i32>,
    pub last_full_sync: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::site_parameters::Entity",
        from = "Column::SiteParameterId",
        to = "super::site_parameters::Column::Id"
    )]
    SiteParameter,
}

impl Related<super::site_parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SiteParameter.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
