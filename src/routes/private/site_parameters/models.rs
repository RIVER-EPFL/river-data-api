use crudcrate::EntityToModels;
use sea_orm::FromQueryResult;
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
    /// Site-level units override. NULL means "no override": every endpoint serving this slot
    /// resolves units through [`SlotDescriptor`], which falls back to the
    /// catalog `default_units`.
    pub display_units: Option<String>,
    /// Stored and returned by the CRUD endpoint; no server-side reader. Kept because existing
    /// rows carry values.
    pub units_name: Option<String>,
    /// Stored and returned by the CRUD endpoint; no server-side reader.
    pub units_min: Option<f64>,
    /// Stored and returned by the CRUD endpoint; no server-side reader.
    pub units_max: Option<f64>,
    /// Display precision, carried to the client by [`SlotDescriptor`]. The API
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
    /// Carried by a slot the chain minted where the site declared the calculation's inputs and
    /// not its output (Q193); cleared by a manager confirming the slot from the site's Parameters
    /// tab.
    #[crudcrate(filterable, sortable, on_create = false)]
    pub needs_review: bool,
    /// How this slot's replicate standard deviation is defined: 'sample' (divisor n-1) or
    /// 'population' (divisor n). NULL is UNDECLARED, not a synonym for 'sample': the sources use
    /// both conventions and which one a slot publishes is a decision, so an undeclared slot is
    /// reported and its population-signature audit holds cannot be waved through.
    /// Excluded from update: a declaration change must recompute the slot's stored samples, so it
    /// goes through `POST /site_parameters/{id}/declare_sd_estimator`, which enqueues the retag.
    #[crudcrate(filterable, exclude(update))]
    pub sd_estimator: Option<String>,
    /// How this site fills the slot: 'manual' (a person types the value) or 'tool' (a
    /// calculation computes it here). The declaration is per site, so one site may measure a
    /// parameter by hand while another computes it; which calculation produces it is the
    /// parameter's group binding, never this column.
    #[crudcrate(filterable, on_create = "manual".to_string())]
    pub entry_mode: String,
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
    /// Site override falling back to the catalog `default_units`. An empty catalog value is no
    /// units at all, so it resolves to `None` rather than an empty string.
    pub units: Option<String>,
    /// The site override alone, unresolved, for clients that distinguish an override from a
    /// fallback.
    pub display_units: Option<String>,
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

/// One member of a group as the apply flow reads it: the catalog parameter, its code, and the role
/// the group gives it.
pub type GroupMember = (Uuid, String, String);

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeclareSdEstimatorRequest {
    /// 'sample' (divisor n-1), 'population' (divisor n), or null to clear the declaration.
    /// Clearing leaves the slot undeclared: new statistics fall back to sample recorded as
    /// 'default', and stored samples keep the estimator they were computed with.
    pub estimator: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DeclareSdEstimatorResponse {
    pub site_parameter_id: Uuid,
    #[schema(required)]
    pub estimator: Option<String>,
    #[schema(required)]
    pub previous: Option<String>,
    /// Samples the retag will recompute; 0 when clearing or nothing disagrees.
    pub samples_affected: i64,
    /// The tracked `sd_estimator_retag` job, present when a recompute was enqueued.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub job_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RetagSdEstimatorRequest {
    /// 'sample' (divisor n-1) or 'population' (divisor n). Every slot in scope must already
    /// declare it: the retag applies a declaration to the stored samples, it does not make one.
    pub estimator: String,
    #[serde(default)]
    pub site_parameter_ids: Vec<Uuid>,
    /// Streams reach their slot through their pairing; an unpaired stream reaches none.
    #[serde(default)]
    pub stream_ids: Vec<Uuid>,
    /// Inclusive bounds on `samples.collected_at`.
    #[serde(default)]
    pub start: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub end: Option<chrono::DateTime<chrono::Utc>>,
    /// Also retag samples whose estimator a person chose for that one instant
    /// (`sd_estimator_source = 'sample'`). Off by default, as the declaration's own retag is.
    #[serde(default)]
    pub override_instants: bool,
    /// Count what the retag would touch and enqueue nothing. The declaration check is skipped,
    /// so a slot can be previewed under the divisor it is about to declare.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RetagSdEstimatorResponse {
    pub estimator: String,
    /// Samples the retag will recompute.
    pub samples_affected: i64,
    /// Samples in scope carrying an instant-chosen estimator that differs from the target:
    /// counted inside `samples_affected` with `override_instants`, skipped without.
    pub instant_decisions: i64,
    /// The tracked `sd_estimator_retag` job, present when something needed recomputing.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub job_id: Option<Uuid>,
}

/// How many samples a declaration change would recompute, by whether the instant declared for
/// itself.
#[derive(FromQueryResult)]
pub struct RetagCounts {
    pub slot_rows: i64,
    pub instant_rows: i64,
}

/// One slot named in a retag refusal: the site, the parameter and whatever it declares, which is
/// the thing the caller has to change.
#[derive(FromQueryResult)]
pub struct UndeclaredRow {
    pub site_name: String,
    pub parameter_name: String,
    pub sd_estimator: Option<String>,
}
