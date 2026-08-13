use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(
    Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "pairing_plans")]
#[crudcrate(
    api_struct = "PairingPlan",
    name_singular = "pairing_plan",
    name_plural = "pairing_plans",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub source_system: String,
    #[crudcrate(filterable, on_create = String::from("draft"))]
    pub status: String,
    pub created_by: Option<String>,
    #[sea_orm(column_type = "JsonBinary")]
    pub summary: serde_json::Value,
    #[sea_orm(column_type = "JsonBinary")]
    pub entries: serde_json::Value,
    #[crudcrate(sortable, exclude(create, update))]
    pub created_at: DateTimeWithTimeZone,
    #[crudcrate(sortable, exclude(create, update))]
    pub applied_at: Option<DateTimeWithTimeZone>,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    #[crudcrate(exclude(create, update))]
    pub apply_result: Option<serde_json::Value>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
