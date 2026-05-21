use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(
    Clone,
    Debug,
    PartialEq,
    DeriveEntityModel,
    Serialize,
    Deserialize,
    EntityToModels,
)]
#[sea_orm(table_name = "sync_commands")]
#[crudcrate(
    api_struct = "SyncCommand",
    name_singular = "sync_command",
    name_plural = "sync_commands",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub service_id: Uuid,
    #[crudcrate(filterable)]
    pub command: String,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub payload: Option<serde_json::Value>,
    #[crudcrate(filterable)]
    pub status: String,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub result: Option<serde_json::Value>,
    #[crudcrate(sortable, exclude(create, update))]
    pub created_at: DateTimeWithTimeZone,
    pub expires_at: DateTimeWithTimeZone,
    pub acknowledged_at: Option<DateTimeWithTimeZone>,
    pub completed_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::sync::services_model::Entity",
        from = "Column::ServiceId",
        to = "crate::routes::private::sync::services_model::Column::Id"
    )]
    SyncService,
}

impl Related<crate::routes::private::sync::services_model::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncService.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
