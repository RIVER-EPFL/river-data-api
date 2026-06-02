use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    DeriveEntityModel,
    serde::Serialize,
    serde::Deserialize,
    EntityToModels,
)]
#[sea_orm(table_name = "projects")]
#[crudcrate(
    api_struct = "Project",
    name_singular = "project",
    name_plural = "projects",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[sea_orm(unique)]
    #[crudcrate(filterable, fulltext, sortable)]
    pub name: String,
    #[crudcrate(filterable)]
    pub data_source: Option<String>,
    pub description: Option<String>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update))]
    pub discovered_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(filterable, on_create = false)]
    pub is_public: bool,
    #[crudcrate(filterable)]
    pub public_slug: Option<String>,
    pub public_api_title: Option<String>,
    pub public_api_description: Option<String>,
    pub public_api_version: Option<String>,
    pub public_contact_email: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "crate::routes::private::sites::Entity")]
    Sites,
}

impl Related<crate::routes::private::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sites.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
