use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

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
    derive_partial_eq
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[sea_orm(unique)]
    #[crudcrate(filterable, fulltext, sortable)]
    pub name: String,
    #[crudcrate(fulltext, sortable)]
    pub display_name: String,
    #[crudcrate(sortable)]
    pub default_units: String,
    #[sea_orm(column_type = "String(StringLen::N(32))")]
    #[crudcrate(filterable, sortable)]
    pub category: String,
    #[sea_orm(column_type = "String(StringLen::N(16))")]
    #[crudcrate(filterable, sortable)]
    pub data_type: String,
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
    #[sea_orm(has_many = "crate::entity::sensors::Entity")]
    Sensors,
    #[sea_orm(has_many = "crate::entity::site_parameters::Entity")]
    SiteParameters,
    #[sea_orm(has_many = "crate::entity::alarm_thresholds::Entity")]
    AlarmThresholds,
    #[sea_orm(has_many = "crate::entity::public_exposed_parameters::Entity")]
    PublicExposedParameters,
    #[sea_orm(has_many = "crate::entity::status_events::Entity")]
    StatusEvents,
    #[sea_orm(has_many = "crate::entity::derived_parameter_sources::Entity")]
    DerivedParameterSources,
}

impl Related<crate::entity::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensors.def()
    }
}

impl Related<crate::entity::site_parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SiteParameters.def()
    }
}

impl Related<crate::entity::alarm_thresholds::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::AlarmThresholds.def()
    }
}

impl Related<crate::entity::public_exposed_parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::PublicExposedParameters.def()
    }
}

impl Related<crate::entity::status_events::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::StatusEvents.def()
    }
}

impl Related<crate::entity::derived_parameter_sources::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DerivedParameterSources.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
