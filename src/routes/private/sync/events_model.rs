use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, EntityToModels)]
#[sea_orm(table_name = "sync_events")]
#[crudcrate(
    api_struct = "SyncEvent",
    name_singular = "sync_event",
    name_plural = "sync_events",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub service_id: Uuid,
    pub command_id: Option<Uuid>,
    #[crudcrate(filterable)]
    pub event_type: String,
    #[crudcrate(filterable)]
    pub status: String,
    pub readings_synced: i64,
    /// Readings the cycle sent that ingest admission dropped.
    #[crudcrate(on_create = 0)]
    pub readings_skipped: i64,
    pub status_events_synced: i64,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub errors: Option<serde_json::Value>,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub log: Option<serde_json::Value>,
    #[crudcrate(sortable)]
    pub started_at: DateTimeWithTimeZone,
    #[crudcrate(sortable)]
    pub completed_at: Option<DateTimeWithTimeZone>,
    pub duration_ms: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::sync::services_model::Entity",
        from = "Column::ServiceId",
        to = "crate::routes::private::sync::services_model::Column::Id"
    )]
    SyncService,
    #[sea_orm(
        belongs_to = "crate::routes::private::sync::commands_model::Entity",
        from = "Column::CommandId",
        to = "crate::routes::private::sync::commands_model::Column::Id"
    )]
    SyncCommand,
}

impl Related<crate::routes::private::sync::services_model::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncService.def()
    }
}

impl Related<crate::routes::private::sync::commands_model::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncCommand.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
