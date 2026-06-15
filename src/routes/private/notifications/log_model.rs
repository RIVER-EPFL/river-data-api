use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "notification_log")]
#[crudcrate(
    api_struct = "NotificationLog",
    name_singular = "notification_log",
    name_plural = "notification_logs",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub alarm_event_id: Option<Uuid>,
    #[crudcrate(filterable)]
    pub kind: String,
    #[crudcrate(filterable)]
    pub channel: String,
    pub recipient: String,
    #[crudcrate(filterable)]
    pub status: String,
    pub error: Option<String>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
