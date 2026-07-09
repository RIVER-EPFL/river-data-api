use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "subprojects")]
#[crudcrate(
    api_struct = "Subproject",
    name_singular = "subproject",
    name_plural = "subprojects",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable, sortable)]
    pub project_id: Uuid,
    #[crudcrate(filterable, fulltext, sortable)]
    pub name: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub description: Option<String>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::projects::Entity",
        from = "Column::ProjectId",
        to = "crate::routes::private::projects::Column::Id"
    )]
    Project,
    #[sea_orm(has_many = "crate::routes::private::sites::Entity")]
    Sites,
}

impl Related<crate::routes::private::projects::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Project.def()
    }
}

impl Related<crate::routes::private::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sites.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
