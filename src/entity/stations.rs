use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "stations")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub zone_id: Option<Uuid>,
    #[sea_orm(unique)]
    pub name: String,
    #[sea_orm(unique)]
    pub vaisala_node_id: i32,
    pub vaisala_path: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub altitude_m: Option<f64>,
    pub created_at: Option<DateTimeWithTimeZone>,
    pub discovered_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::zones::Entity",
        from = "Column::ZoneId",
        to = "super::zones::Column::Id"
    )]
    Zone,
    #[sea_orm(has_many = "super::sensors::Entity")]
    Sensors,
}

impl Related<super::zones::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Zone.def()
    }
}

impl Related<super::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensors.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
