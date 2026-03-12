use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

use crate::services::operations::DerivedParameterDefinitionOperations;

#[derive(
    Clone, Debug, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "derived_parameter_definitions")]
#[crudcrate(
    api_struct = "DerivedParameterDefinition",
    name_singular = "derived_parameter_definition",
    name_plural = "derived_parameter_definitions",
    generate_router,
    operations = DerivedParameterDefinitionOperations
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[sea_orm(unique)]
    #[crudcrate(filterable, fulltext)]
    pub name: String,
    #[crudcrate(fulltext)]
    pub display_name: String,
    pub units: String,
    pub formula: String,
    pub description: Option<String>,
    #[crudcrate(exclude(create, update))]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update), join(one, all))]
    pub sources: Vec<super::derived_parameter_sources::DerivedParameterSource>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::site_parameters::Entity")]
    SiteParameters,
    #[sea_orm(has_many = "super::derived_parameter_sources::Entity")]
    DerivedParameterSources,
}

impl Related<super::site_parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SiteParameters.def()
    }
}

impl Related<super::derived_parameter_sources::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DerivedParameterSources.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
