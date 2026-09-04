use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;

use super::operations::ParameterGroupOperations;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "parameter_groups")]
#[crudcrate(
    api_struct = "ParameterGroup",
    name_singular = "parameter_group",
    name_plural = "parameter_groups",
    generate_router,
    derive_partial_eq,
    operations = ParameterGroupOperations
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[sea_orm(unique)]
    #[crudcrate(filterable, fulltext, sortable)]
    pub code: String,
    #[crudcrate(fulltext, sortable)]
    pub label: String,
    #[crudcrate(sortable)]
    pub description: Option<String>,
    /// Where the group sits in the portal's category order.
    #[crudcrate(filterable, sortable, on_create = 0)]
    pub ordinal: i32,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::member_model::Entity")]
    Members,
}

impl Related<super::member_model::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Members.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
