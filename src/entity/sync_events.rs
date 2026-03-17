use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "sync_events")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub service_id: Uuid,
    pub command_id: Option<Uuid>,
    pub event_type: String,
    pub status: String,
    pub readings_synced: i64,
    pub status_events_synced: i64,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub errors: Option<serde_json::Value>,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub log: Option<serde_json::Value>,
    pub started_at: DateTimeWithTimeZone,
    pub completed_at: Option<DateTimeWithTimeZone>,
    pub duration_ms: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::sync_services::Entity",
        from = "Column::ServiceId",
        to = "super::sync_services::Column::Id"
    )]
    SyncService,
    #[sea_orm(
        belongs_to = "super::sync_commands::Entity",
        from = "Column::CommandId",
        to = "super::sync_commands::Column::Id"
    )]
    SyncCommand,
}

impl Related<super::sync_services::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncService.def()
    }
}

impl Related<super::sync_commands::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncCommand.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
