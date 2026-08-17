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
    // every command, never cached here as authority. See notifications::authz.
    #[crudcrate(filterable)]
    pub linked_keycloak_sub: String,
    // NULL until the user claims the row with /start <code>.
    #[crudcrate(filterable)]
    pub telegram_chat_id: Option<i64>,
    #[crudcrate(on_create = true)]
    pub receive_alerts: bool,
    #[crudcrate(filterable, on_create = true)]
    pub is_active: bool,
    // Holds the link open against *inactivity only* (a shared operations chat, a dormant field
    // season). Never shields a revoked Keycloak user: reconcile::sweep deactivates those before it
    // consults this. Administrator-only, enforced in the /notifications/me handler.
    #[crudcrate(filterable, on_create = false)]
    pub expiry_exempt: bool,
    // Server-managed: set by the link endpoint, cleared on claim. Not client-writable.
    #[crudcrate(exclude(create, update))]
    pub link_code: Option<String>,
    #[crudcrate(exclude(create, update))]
    pub link_code_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update))]
    pub last_verified_at: Option<chrono::DateTime<chrono::Utc>>,
    // The second clock. Telegram activity (sent or received) moves `last_verified_at`, which an
    // alert stream keeps alive on its own; only an authenticated portal request moves this one, so
    // a link cannot outlive proof that its owner still holds the Keycloak account.
    #[crudcrate(exclude(create, update))]
    pub last_attested_at: Option<chrono::DateTime<chrono::Utc>>,
    // The Telegram account that claimed the link. An inbound message whose sender disagrees is
    // refused, so the binding is to a person rather than only to a chat.
    #[crudcrate(exclude(create, update))]
    pub telegram_user_id: Option<i64>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[crudcrate(exclude(create, update))]
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
