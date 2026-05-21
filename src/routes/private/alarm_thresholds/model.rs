use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "alarm_thresholds")]
#[crudcrate(
    api_struct = "AlarmThreshold",
    name_singular = "alarm_threshold",
    name_plural = "alarm_thresholds",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub parameter_id: Uuid,
    #[crudcrate(filterable)]
    pub site_id: Option<Uuid>,
    #[crudcrate(filterable)]
    pub alarm_type: String,
    pub warning_min: Option<f64>,
    pub warning_max: Option<f64>,
    pub alarm_min: Option<f64>,
    pub alarm_max: Option<f64>,
    pub description: Option<String>,
    pub string_alarm_values: Option<serde_json::Value>,
    pub string_warning_values: Option<serde_json::Value>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update))]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::entity::parameters::Entity",
        from = "Column::ParameterId",
        to = "crate::entity::parameters::Column::Id"
    )]
    Parameter,
    #[sea_orm(
        belongs_to = "crate::entity::sites::Entity",
        from = "Column::SiteId",
        to = "crate::entity::sites::Column::Id"
    )]
    Site,
}

impl Related<crate::entity::parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Parameter.def()
    }
}

impl Related<crate::entity::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Site.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
