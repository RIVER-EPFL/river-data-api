use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "sync_services")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub service_type: String,
    pub instance_id: String,
    pub status: String,
    pub current_operation: Option<String>,
    pub last_heartbeat: Option<DateTimeWithTimeZone>,
    pub last_sync_completed_at: Option<DateTimeWithTimeZone>,
    pub last_error: Option<String>,
    pub created_at: DateTimeWithTimeZone,
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::sync_commands::Entity")]
    SyncCommands,
    #[sea_orm(has_many = "super::sync_events::Entity")]
    SyncEvents,
    #[sea_orm(has_many = "super::sync_service_credentials::Entity")]
    SyncServiceCredentials,
    #[sea_orm(has_many = "super::sync_service_tokens::Entity")]
    SyncServiceTokens,
}

impl Related<super::sync_commands::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncCommands.def()
    }
}

impl Related<super::sync_events::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncEvents.def()
    }
}

impl Related<super::sync_service_credentials::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncServiceCredentials.def()
    }
}

impl Related<super::sync_service_tokens::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncServiceTokens.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
