use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "sync_service_credentials")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    #[sea_orm(unique)]
    pub client_id: String,
    pub client_secret_hash: String,
    pub service_type: String,
    pub service_id: Option<Uuid>,
    pub revoked: bool,
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
