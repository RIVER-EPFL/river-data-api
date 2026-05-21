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
#[sea_orm(table_name = "sync_services")]
#[crudcrate(
    api_struct = "SyncService",
    name_singular = "sync_service",
    name_plural = "sync_services",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub service_type: String,
    #[crudcrate(filterable)]
    pub instance_id: String,
    #[crudcrate(filterable)]
    pub status: String,
    pub current_operation: Option<String>,
    #[crudcrate(sortable, exclude(create, update))]
    pub last_heartbeat: Option<DateTimeWithTimeZone>,
    pub last_sync_completed_at: Option<DateTimeWithTimeZone>,
    pub last_error: Option<String>,
    #[crudcrate(sortable, exclude(create, update))]
    pub created_at: DateTimeWithTimeZone,
    #[crudcrate(sortable, exclude(create, update))]
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "crate::entity::sync_commands::Entity")]
    SyncCommands,
    #[sea_orm(has_many = "crate::entity::sync_events::Entity")]
    SyncEvents,
    #[sea_orm(has_many = "crate::entity::sync_service_credentials::Entity")]
    SyncServiceCredentials,
    #[sea_orm(has_many = "crate::entity::sync_service_tokens::Entity")]
    SyncServiceTokens,
}

impl Related<crate::entity::sync_commands::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncCommands.def()
    }
}

impl Related<crate::entity::sync_events::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncEvents.def()
    }
}

impl Related<crate::entity::sync_service_credentials::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncServiceCredentials.def()
    }
}

impl Related<crate::entity::sync_service_tokens::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncServiceTokens.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
