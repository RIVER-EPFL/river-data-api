use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;

use super::service::ParameterOperations;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
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
    #[crudcrate(filterable, on_create = Vec::new())]
    pub aliases: Vec<String>,
    /// Set on catalog entries created mechanically (the analyte seed); cleared by a manager
    /// confirming or merging the parameter.
    #[crudcrate(filterable, on_create = false)]
    pub needs_review: bool,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The calculation that minted this row and no longer publishes it, where one did: a formula
    /// ticked as a step still names this code while it publishes nothing, so the catalogue offers a
    /// name nothing computes. Read from the formulas on every fetch, never stored.
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub unpublished_by: Option<String>,
    /// The decommissioned calculation that last published this row, while no live calculation
    /// publishes it (Q299). Read from the formulas on every fetch, never stored.
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub decommissioned_by: Option<DecommissionedBy>,
}

/// A decommissioned calculation that published a parameter, and when it was decommissioned.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct DecommissionedBy {
    pub tool_script_id: Uuid,
    pub calculation: String,
    pub at: chrono::DateTime<chrono::Utc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "crate::routes::private::site_parameters::Entity")]
    SiteParameters,
    #[sea_orm(has_many = "crate::routes::private::alarms::models::Entity")]
    AlarmThresholds,
    #[sea_orm(has_many = "crate::routes::private::readings::status_events::Entity")]
    StatusEvents,
    #[sea_orm(has_many = "crate::routes::private::derived_parameters::models::source::Entity")]
    DerivedParameterSources,
}

impl Related<crate::routes::private::site_parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SiteParameters.def()
    }
}

impl Related<crate::routes::private::alarms::models::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::AlarmThresholds.def()
    }
}

impl Related<crate::routes::private::readings::status_events::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::StatusEvents.def()
    }
}

impl Related<crate::routes::private::derived_parameters::models::source::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DerivedParameterSources.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
