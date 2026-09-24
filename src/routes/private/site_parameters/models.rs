use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

use super::service::SiteParameterOperations;

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
    /// Display precision, carried to the client by [`SlotDescriptor`]. The API
    /// serves full precision and the client formats, so a change here never rewrites a value.
    pub decimal_places: Option<i16>,
    pub sample_interval_sec: Option<i32>,
    // `on_create` is where the create-time default lives: the field stays in the create model so
    // the client's value is honoured, and an omitted field takes the expression rather than NULL.
    #[crudcrate(filterable, on_create = true)]
    pub is_active: Option<bool>,
    #[crudcrate(filterable, on_create = false)]
    pub is_public: Option<bool>,
    /// Carried by a slot the chain minted before Q325 made adding a calculation the only thing that
    /// runs it; cleared by a manager confirming the slot or applying the calculation at the site.
    /// A slot carrying it does not make a calculation run there (Q317).
    #[crudcrate(filterable, sortable, on_create = false)]
    pub needs_review: bool,
    /// How this site fills the slot: 'manual' (a person types the value) or 'tool' (a
    /// calculation computes it here). The declaration is per site, so one site may measure a
    /// parameter by hand while another computes it; which calculation produces it is the
    /// parameter's group binding, never this column.
    #[crudcrate(filterable, on_create = "manual".to_string())]
    pub entry_mode: String,
    /// The cadence this site fills the slot at: 'high' (a stream carries it, and the continuous
    /// engine computes it there) or 'low' (a person records it at a visit, and the chain computes
    /// it from that visit's values). `readings` holds one row per slot instant, so the
    /// declaration is what keeps the two engines off each other's rows.
    #[crudcrate(filterable, on_create = "high".to_string())]
    pub cadence: String,
    /// The instrument that measures this slot, declared here rather than inferred at the write.
    /// NULL is undeclared: a value entered there takes the entry channel's own instrument, which
    /// is a marker for a slot nobody has declared and not a statement about what measured it.
    #[crudcrate(filterable)]
    pub instrument_sensor_id: Option<Uuid>,
    /// Stored and returned by the CRUD endpoint; no server-side reader. Derived-formula variables
    /// bind through `derived_parameter_sources.variable_name`, never through this column.
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub variable_mappings: Option<serde_json::Value>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update))]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Stamped by every sync path that mints a slot, and by nothing else, so it is what says a
    /// row arrived from a source rather than by hand.
    #[crudcrate(exclude(create, update), filterable, sortable)]
    pub discovered_at: Option<chrono::DateTime<chrono::Utc>>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update), join(one, all, depth = 1))]
    pub parameter: Vec<crate::routes::private::parameters::Parameter>,
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

impl ActiveModelBehavior for ActiveModel {}

/// The global catalog row a slot points at.
#[derive(Debug, Clone)]
pub struct CatalogParameter {
    pub code: String,
    pub name: String,
    pub default_units: String,
}

/// Everything an endpoint needs to label one slot, with the catalog fallbacks already applied.
///
/// One `(site, parameter)` slot must describe itself identically whichever endpoint is asked, so
/// the catalog fallbacks (units, name, sensor type) are resolved in [`SlotDescriptor::resolve`] and
/// nowhere else. Endpoints differ in the JSON field names they publish, not in the values, so this
/// type exposes every resolved value and each response struct copies the ones it publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotDescriptor {
    /// `site_parameters.id`.
    pub id: Uuid,
    /// Global catalog parameter id.
    pub parameter_id: Uuid,
    /// Catalog `code`, empty when the catalog row is missing.
    pub code: String,
    /// Catalog `name`, absent when the catalog row is missing. Published as `display_name` by the
    /// series endpoints.
    pub catalog_name: Option<String>,
    /// The slot's own `name`. Published as `name` by the series endpoints.
    pub slot_name: String,
    /// Catalog name falling back to the slot name. Published as `name` by the site detail and
    /// parameter-list projections.
    pub name: String,
    /// Slot `sensor_type`, falling back to the slot name when unset.
    pub sensor_type: String,
    /// The catalog `default_units`. An empty catalog value is no units at all, so it resolves to
    /// `None` rather than an empty string.
    pub units: Option<String>,
    /// Display precision the client formats with. The served values keep full precision.
    pub decimal_places: Option<i16>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyGroupRequest {
    pub group_id: Uuid,
    /// The instrument that measures these slots at this site, written onto every row this call
    /// creates. Declared per site parameter, never per group: a later row may say otherwise
    /// (M111). Omitted leaves the slots undeclared, which is a legitimate state.
    #[serde(default)]
    pub instrument_sensor_id: Option<Uuid>,
    /// Report what would be created without creating it.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AppliedSlot {
    pub parameter_id: Uuid,
    pub parameter_code: String,
    /// The group's role for this member: `measured`, `entry_only` or `output`.
    pub role: String,
    /// The slot's id, absent on a dry run.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub site_parameter_id: Option<Uuid>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ApplyGroupResponse {
    pub site_id: Uuid,
    pub group_id: Uuid,
    pub dry_run: bool,
    /// The slots this call created, or would create.
    pub created: Vec<AppliedSlot>,
    /// Members the site already carried, left exactly as they are.
    pub existing: Vec<AppliedSlot>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyCalculationRequest {
    /// The tool script the calculation is authored as.
    pub calculation_id: Uuid,
    /// Report what would be created, and what is missing, without creating anything.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ApplyCalculationResponse {
    pub site_id: Uuid,
    pub calculation_id: Uuid,
    pub calculation_name: String,
    pub dry_run: bool,
    /// Read inputs the site already declares.
    pub inputs_present: Vec<AppliedSlot>,
    /// Read inputs the site does not declare. Non-empty refuses the apply.
    pub inputs_missing: Vec<AppliedSlot>,
    /// Output slots the site already carries, left exactly as they are.
    pub outputs_existing: Vec<AppliedSlot>,
    /// Output slots this call created, or would create.
    pub outputs_created: Vec<AppliedSlot>,
}

/// One member of a group as the apply flow reads it: the catalog parameter, its code, and the role
/// the group gives it.
pub type GroupMember = (Uuid, String, String);
