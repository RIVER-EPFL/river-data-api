use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "derived_parameter_sources")]
#[crudcrate(
    api_struct = "DerivedParameterSource",
    name_singular = "derived_parameter_source",
    name_plural = "derived_parameter_sources",
    generate_router,
    derive_partial_eq
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub derived_definition_id: Uuid,
    #[crudcrate(filterable)]
    pub parameter_id: Uuid,
    pub variable_name: String,
    #[crudcrate(exclude(create, update))]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::parameters::derived::definition_model::Entity",
        from = "Column::DerivedDefinitionId",
        to = "crate::routes::private::parameters::derived::definition_model::Column::Id"
    )]
    DerivedParameterDefinition,
    #[sea_orm(
        belongs_to = "crate::routes::private::parameters::Entity",
        from = "Column::ParameterId",
        to = "crate::routes::private::parameters::Column::Id"
    )]
    Parameter,
}

impl Related<crate::routes::private::parameters::derived::definition_model::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DerivedParameterDefinition.def()
    }
}

impl Related<crate::routes::private::parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Parameter.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
