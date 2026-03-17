use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "source_mappings")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub source_system: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub entity_type: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub source_key: String,
    pub entity_id: Uuid,
    pub source_name: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
