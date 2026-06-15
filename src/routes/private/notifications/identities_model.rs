use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "telegram_identities")]
#[crudcrate(
    api_struct = "TelegramIdentity",
    name_singular = "telegram_identity",
    name_plural = "telegram_identities",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    // The Keycloak user this chat speaks for. The effective role is resolved live from this sub on
    // every command — never cached here as authority. See notifications::authz.
    #[crudcrate(filterable)]
    pub linked_keycloak_sub: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    // NULL until the user claims the row with /start <code>.
    #[crudcrate(filterable)]
    pub telegram_chat_id: Option<i64>,
    pub telegram_username: Option<String>,
    #[crudcrate(on_create = true)]
    pub receive_alerts: bool,
    #[crudcrate(filterable, on_create = true)]
    pub is_active: bool,
    // Server-managed: set by the link endpoint, cleared on claim. Not client-writable.
    #[crudcrate(exclude(create, update))]
    pub link_code: Option<String>,
    #[crudcrate(exclude(create, update))]
    pub link_code_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update))]
    pub last_verified_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[crudcrate(exclude(create, update))]
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
