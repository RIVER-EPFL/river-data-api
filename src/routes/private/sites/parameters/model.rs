use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

use super::operations::SiteParameterOperations;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "site_parameters")]
#[crudcrate(
    api_struct = "SiteParameter",
    name_singular = "site_parameter",
    name_plural = "site_parameters",
    generate_router,
    operations = SiteParameterOperations
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub site_id: Uuid,
    #[crudcrate(filterable)]
    pub parameter_id: Uuid,
    #[crudcrate(filterable, fulltext, sortable, on_create = String::new())]
    pub name: String,
    #[crudcrate(filterable, on_create = String::new())]
    pub sensor_type: String,
    /// Site-level units override. NULL means "no override": every endpoint serving this slot
    /// resolves units through [`super::descriptor::SlotDescriptor`], which falls back to the
    /// catalog `default_units`.
    pub display_units: Option<String>,
    /// Stored and returned by the CRUD endpoint; no server-side reader. Kept because existing
    /// rows carry values.
    pub units_name: Option<String>,
    /// Stored and returned by the CRUD endpoint; no server-side reader.
    pub units_min: Option<f64>,
    /// Stored and returned by the CRUD endpoint; no server-side reader.
    pub units_max: Option<f64>,
    /// Display precision, carried to the client by [`super::descriptor::SlotDescriptor`]. The API
    /// serves full precision and the client formats, so a change here never rewrites a value.
    pub decimal_places: Option<i16>,
    pub channel_id: Option<i32>,
    pub sample_interval_sec: Option<i32>,
    // `on_create` is where the create-time default lives: the field stays in the create model so
    // the client's value is honoured, and an omitted field takes the expression rather than NULL.
    #[crudcrate(filterable, on_create = true)]
    pub is_active: Option<bool>,
    #[crudcrate(filterable, on_create = false)]
    pub is_public: Option<bool>,
    /// How this slot's replicate standard deviation is defined: 'sample' (divisor n-1) or
    /// 'population' (divisor n). NULL is UNDECLARED, not a synonym for 'sample': the sources use
    /// both conventions and which one a slot publishes is a decision, so an undeclared slot is
    /// reported and its population-signature audit holds cannot be waved through.
    /// Excluded from update: a declaration change must recompute the slot's stored samples, so it
    /// goes through `POST /site_parameters/{id}/declare_sd_estimator`, which enqueues the retag.
    #[crudcrate(filterable, exclude(update))]
    pub sd_estimator: Option<String>,
    #[crudcrate(filterable)]
    pub is_derived: Option<bool>,
    #[crudcrate(filterable)]
    pub derived_definition_id: Option<Uuid>,
    /// Stored and returned by the CRUD endpoint; no server-side reader. Derived-formula variables
    /// bind through `derived_parameter_sources.variable_name`, never through this column.
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub variable_mappings: Option<serde_json::Value>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update))]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update))]
    pub discovered_at: Option<chrono::DateTime<chrono::Utc>>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update), join(one, all, depth = 1))]
    pub parameter: Vec<crate::routes::private::parameters::Parameter>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub derived_definition: Option<
        crate::routes::private::parameters::derived::definition_model::DerivedParameterDefinition,
    >,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::sites::Entity",
        from = "Column::SiteId",
        to = "crate::routes::private::sites::Column::Id"
    )]
    Site,
    #[sea_orm(
        belongs_to = "crate::routes::private::parameters::Entity",
        from = "Column::ParameterId",
        to = "crate::routes::private::parameters::Column::Id"
    )]
    Parameter,
    #[sea_orm(
        belongs_to = "crate::routes::private::parameters::derived::definition_model::Entity",
        from = "Column::DerivedDefinitionId",
        to = "crate::routes::private::parameters::derived::definition_model::Column::Id"
    )]
    DerivedParameterDefinition,
}

impl Related<crate::routes::private::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Site.def()
    }
}

impl Related<crate::routes::private::parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Parameter.def()
    }
}

impl Related<crate::routes::private::parameters::derived::definition_model::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DerivedParameterDefinition.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
