use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, EntityToModels)]
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
    #[crudcrate(filterable, exclude(create), on_create = false)]
    pub paused: bool,
    pub current_operation: Option<String>,
    /// Operator-set scheduled sync cadence in seconds. NULL leaves the service on its own
    /// `SYNC_INTERVAL_SECONDS`. Set through `PATCH /sync/services/{id}`, which enforces the
    /// minimum the runner floors at; generic CRUD must not write it around that check.
    #[crudcrate(exclude(create, update))]
    pub sync_interval_secs: Option<i32>,
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
    #[sea_orm(has_many = "crate::routes::private::sync::commands_model::Entity")]
    SyncCommands,
    #[sea_orm(has_many = "crate::routes::private::sync::events_model::Entity")]
    SyncEvents,
    #[sea_orm(has_many = "crate::routes::private::sync::credentials_model::Entity")]
    SyncServiceCredentials,
    #[sea_orm(has_many = "crate::routes::private::sync::tokens_model::Entity")]
    SyncServiceTokens,
}

impl Related<crate::routes::private::sync::commands_model::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncCommands.def()
    }
}

impl Related<crate::routes::private::sync::events_model::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncEvents.def()
    }
}

impl Related<crate::routes::private::sync::credentials_model::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncServiceCredentials.def()
    }
}

impl Related<crate::routes::private::sync::tokens_model::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncServiceTokens.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
