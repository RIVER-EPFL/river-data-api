use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "zones")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    #[sea_orm(unique)]
    pub name: String,
    pub vaisala_path: Option<String>,
    pub description: Option<String>,
    pub created_at: Option<DateTimeWithTimeZone>,
    pub discovered_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::stations::Entity")]
    Stations,
}

impl Related<super::stations::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Stations.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
