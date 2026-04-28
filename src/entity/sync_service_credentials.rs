use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, EntityToModels)]
#[sea_orm(table_name = "sync_service_credentials")]
#[crudcrate(
    api_struct = "SyncServiceCredential",
    name_singular = "sync_service_credential",
    name_plural = "sync_service_credentials",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[sea_orm(unique)]
    #[crudcrate(filterable)]
    pub client_id: String,
    #[crudcrate(exclude(create, update))]
    pub client_secret_hash: String,
    #[crudcrate(filterable)]
    pub service_type: String,
    #[crudcrate(filterable)]
    pub service_id: Option<Uuid>,
    #[crudcrate(filterable)]
    pub revoked: bool,
    #[crudcrate(sortable, exclude(create, update))]
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::sync_services::Entity",
        from = "Column::ServiceId",
        to = "super::sync_services::Column::Id"
    )]
    SyncService,
}

impl Related<super::sync_services::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncService.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
