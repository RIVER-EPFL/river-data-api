use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "pairing_plans")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub source_system: String,
    pub status: String,
    pub created_by: Option<String>,
    #[sea_orm(column_type = "JsonBinary")]
    pub summary: serde_json::Value,
    #[sea_orm(column_type = "JsonBinary")]
    pub entries: serde_json::Value,
    pub created_at: DateTimeWithTimeZone,
    pub applied_at: Option<DateTimeWithTimeZone>,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub apply_result: Option<serde_json::Value>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
