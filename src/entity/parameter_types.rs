use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels)]
#[sea_orm(table_name = "parameter_types")]
#[crudcrate(
    api_struct = "ParameterType",
    name_singular = "parameter_type",
    name_plural = "parameter_types",
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
    pub default_units: String,
    pub description: Option<String>,
    #[crudcrate(exclude(create, update))]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::sensors::Entity")]
    Sensors,
    #[sea_orm(has_many = "super::parameters::Entity")]
    Parameters,
}

impl Related<super::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensors.def()
    }
}

impl Related<super::parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Parameters.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
