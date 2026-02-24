use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels)]
#[sea_orm(table_name = "derived_parameter_definitions")]
#[crudcrate(
    api_struct = "DerivedParameterDefinition",
    name_singular = "derived_parameter_definition",
    name_plural = "derived_parameter_definitions",
    generate_router
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
    #[sea_orm(column_type = "JsonBinary")]
    pub required_parameter_types: serde_json::Value,
    #[crudcrate(exclude(create, update))]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::parameters::Entity")]
    Parameters,
}

impl Related<super::parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Parameters.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
