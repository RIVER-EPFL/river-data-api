use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels)]
#[sea_orm(table_name = "data_imports")]
#[crudcrate(
    api_struct = "DataImport",
    name_singular = "data_import",
    name_plural = "data_imports",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub project_id: Option<Uuid>,
    #[crudcrate(filterable)]
    pub source_type: String,
    pub file_name: Option<String>,
    #[crudcrate(filterable)]
    pub status: String,
    #[crudcrate(exclude(create, update))]
    pub rows_imported: Option<i32>,
    #[crudcrate(exclude(create, update))]
    pub rows_failed: Option<i32>,
    #[crudcrate(exclude(create, update))]
    pub error_message: Option<String>,
    #[crudcrate(exclude(create, update), sortable)]
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update), sortable)]
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update))]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_by: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::projects::Entity",
        from = "Column::ProjectId",
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
