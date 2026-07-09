use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

use super::operations::ParameterOperations;

#[derive(
    Clone,
    Debug,
    PartialEq,
    DeriveEntityModel,
    serde::Serialize,
    serde::Deserialize,
    EntityToModels,
)]
#[sea_orm(table_name = "parameters")]
#[crudcrate(
    api_struct = "Parameter",
    name_singular = "parameter",
    name_plural = "parameters",
    generate_router,
    derive_partial_eq,
    operations = ParameterOperations
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[sea_orm(unique)]
    #[crudcrate(filterable, fulltext, sortable)]
    pub code: String,
    #[crudcrate(fulltext, sortable)]
    pub name: String,
    #[crudcrate(sortable, on_create = String::new())]
    pub default_units: String,
    #[sea_orm(column_type = "String(StringLen::N(32))")]
    #[crudcrate(filterable, sortable, on_create = String::from("measurement"))]
    pub category: String,
    #[crudcrate(sortable)]
    pub description: Option<String>,
    #[crudcrate(filterable)]
    pub aliases: Vec<String>,
    pub default_warning_min: Option<f64>,
    pub default_warning_max: Option<f64>,
    pub default_alarm_min: Option<f64>,
    pub default_alarm_max: Option<f64>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "crate::routes::private::site_parameters::Entity")]
    SiteParameters,
    #[sea_orm(has_many = "crate::routes::private::alarm_thresholds::Entity")]
    AlarmThresholds,
    #[sea_orm(has_many = "crate::routes::private::status_events::Entity")]
    StatusEvents,
    #[sea_orm(has_many = "crate::routes::private::derived_parameters::source_model::Entity")]
    DerivedParameterSources,
}

impl Related<crate::routes::private::site_parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SiteParameters.def()
    }
}

impl Related<crate::routes::private::alarm_thresholds::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::AlarmThresholds.def()
    }
}

impl Related<crate::routes::private::status_events::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::StatusEvents.def()
    }
}

impl Related<crate::routes::private::derived_parameters::source_model::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DerivedParameterSources.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
