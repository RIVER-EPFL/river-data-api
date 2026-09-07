use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;

use super::operations::ConstantOperations;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "constants")]
#[crudcrate(
    api_struct = "Constant",
    name_singular = "constant",
    name_plural = "constants",
    generate_router,
    operations = ConstantOperations
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[sea_orm(unique)]
    #[crudcrate(filterable, sortable)]
    pub name: String,
    pub value: f64,
    pub units: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub description: Option<String>,
    #[crudcrate(exclude(create, update))]
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
