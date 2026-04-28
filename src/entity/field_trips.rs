use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "field_trips")]
#[crudcrate(
    api_struct = "FieldTrip",
    name_singular = "field_trip",
    name_plural = "field_trips",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable, sortable)]
    pub date: chrono::NaiveDate,
    #[sea_orm(column_type = "Text", nullable)]
    pub participants: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub notes: Option<String>,
    pub created_by: Option<String>,
    #[crudcrate(exclude(create, update))]
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
