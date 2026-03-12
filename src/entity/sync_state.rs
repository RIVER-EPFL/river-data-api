use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    DeriveEntityModel,
    Serialize,
    Deserialize,
    EntityToModels,
)]
#[sea_orm(table_name = "sync_state")]
#[crudcrate(
    api_struct = "SyncState",
    name_singular = "sync_state",
    name_plural = "sync_states",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(create))]
    pub site_parameter_id: Uuid,
    #[crudcrate(sortable)]
    pub last_data_time: Option<DateTimeWithTimeZone>,
    #[crudcrate(sortable)]
    pub last_sync_attempt: Option<DateTimeWithTimeZone>,
    #[crudcrate(filterable)]
    pub sync_status: Option<String>,
    pub error_message: Option<String>,
    pub retry_count: Option<i32>,
    #[crudcrate(sortable)]
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
