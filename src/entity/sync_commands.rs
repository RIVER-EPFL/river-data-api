use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "sync_commands")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub service_id: Uuid,
    pub command: String,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub payload: Option<serde_json::Value>,
    pub status: String,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub result: Option<serde_json::Value>,
    pub created_at: DateTimeWithTimeZone,
    pub expires_at: DateTimeWithTimeZone,
    pub acknowledged_at: Option<DateTimeWithTimeZone>,
    pub completed_at: Option<DateTimeWithTimeZone>,
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
