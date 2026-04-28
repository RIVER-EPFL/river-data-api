use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

use crate::services::operations::ApiTokenOperations;

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
    #[sea_orm(unique)]
    #[crudcrate(exclude(create, update))]
    pub token_hash: String,
    #[crudcrate(filterable)]
    pub project_scope: Option<Uuid>,
    #[sea_orm(column_type = "JsonBinary")]
    pub permissions: serde_json::Value,
    #[crudcrate(filterable)]
    pub is_active: bool,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(sortable)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update), sortable)]
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_by: Option<String>,
    #[sea_orm(ignore)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub raw_token: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::projects::Entity",
        from = "Column::ProjectScope",
        to = "super::projects::Column::Id"
    )]
    Project,
}

impl Related<super::projects::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Project.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
