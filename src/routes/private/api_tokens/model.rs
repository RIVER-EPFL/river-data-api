use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

use super::operations::ApiTokenOperations;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "api_tokens")]
#[crudcrate(
    api_struct = "ApiToken",
    name_singular = "api_token",
    name_plural = "api_tokens",
    generate_router,
    operations = ApiTokenOperations
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable, fulltext, sortable)]
    pub name: String,
    /// Per-key allocation label, which external client/logger this key was issued to.
    #[crudcrate(filterable, fulltext)]
    pub description: Option<String>,
    /// Argon2id PHC hash of the token secret. Excluded from create/update (set on mint).
    #[sea_orm(unique)]
    #[crudcrate(exclude(create, update, list))]
    pub token_hash: String,
    /// Non-secret indexed lookup key (`rvd_<token_prefix>_<secret>`); set on mint.
    #[crudcrate(exclude(create, update), sortable)]
    pub token_prefix: String,
    #[crudcrate(filterable)]
    pub project_scope: Option<Uuid>,
    #[sea_orm(column_type = "JsonBinary")]
    pub permissions: serde_json::Value,
    #[crudcrate(filterable, on_create = true)]
    pub is_active: bool,
    /// Optional per-token request ceiling (requests/second). NULL = unlimited.
    pub rate_limit_per_second: Option<i32>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(sortable)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update), sortable)]
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_by: Option<String>,
    /// The one-time secret, populated only in the create/rotate response. Never stored.
    #[sea_orm(ignore)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub token: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::projects::Entity",
        from = "Column::ProjectScope",
        to = "crate::routes::private::projects::Column::Id"
    )]
    Project,
}

impl Related<crate::routes::private::projects::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Project.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
