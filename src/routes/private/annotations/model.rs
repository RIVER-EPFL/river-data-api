use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "annotations")]
#[crudcrate(
    api_struct = "Annotation",
    name_singular = "annotation",
    name_plural = "annotations",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub site_id: Uuid,
    #[crudcrate(filterable)]
    pub parameter_id: Uuid,
    #[crudcrate(sortable)]
    pub start_time: chrono::DateTime<chrono::Utc>,
    #[crudcrate(sortable)]
    pub end_time: chrono::DateTime<chrono::Utc>,
    #[crudcrate(fulltext)]
    pub text: String,
    #[crudcrate(filterable)]
    pub category: String,
    pub created_by: Option<String>,
    #[crudcrate(exclude(create, update))]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::entity::sites::Entity",
        from = "Column::SiteId",
        to = "crate::entity::sites::Column::Id"
    )]
    Site,
    #[sea_orm(
        belongs_to = "crate::entity::parameters::Entity",
        from = "Column::ParameterId",
        to = "crate::entity::parameters::Column::Id"
    )]
    Parameter,
}

impl Related<crate::entity::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Site.def()
    }
}

impl Related<crate::entity::parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Parameter.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
