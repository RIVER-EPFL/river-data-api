//! The collection event entity, and the request and response shapes the visits grid, the visit
//! list and the event detail are served in.

use chrono::{DateTime, Utc};
use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::service::CollectionEventOperations;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "collection_events")]
#[crudcrate(
    api_struct = "CollectionEvent",
    name_singular = "collection_event",
    name_plural = "collection_events",
    generate_router,
    operations = CollectionEventOperations
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable, sortable)]
    pub site_id: Uuid,
    #[crudcrate(filterable, sortable)]
    pub collected_at: chrono::DateTime<chrono::Utc>,
    /// How the event came to exist. CRUD creation is always a person staging a visit; the sync
    /// attach path writes `portal_sync` rows directly.
    #[crudcrate(filterable, exclude(create, update), on_create = "manual".to_string())]
    pub source: String,
    pub created_by: Option<String>,
    pub notes: Option<String>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[crudcrate(exclude(create, update))]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::sites::Entity",
        from = "Column::SiteId",
        to = "crate::routes::private::sites::Column::Id"
    )]
    Site,
}

impl Related<crate::routes::private::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Site.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

#[derive(Debug, Serialize, ToSchema)]
pub struct EnqueuedJobResponse {
    #[schema(required)]
    pub job_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StageEventRequest {
    pub site_id: Uuid,
    pub collected_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// A trip: one visit per site named, all at one instant.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StageEventsRequest {
    pub site_ids: Vec<Uuid>,
    pub collected_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Serialize, ToSchema, sea_orm::FromQueryResult)]
pub struct StagedEvent {
    pub id: Uuid,
    pub site_id: Uuid,
    pub collected_at: chrono::DateTime<chrono::Utc>,
    pub source: String,
    #[schema(required)]
    pub created_by: Option<String>,
    #[schema(required)]
    pub notes: Option<String>,
    /// False when the visit already stood at this instant, so a second tool joins it.
    pub created: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EventRecomputeRequest {
    /// Recompute every visit at this site.
    #[serde(default)]
    pub site_id: Option<Uuid>,
    /// Visits collected at or after this instant.
    #[serde(default)]
    pub start: Option<chrono::DateTime<chrono::Utc>>,
    /// Visits collected at or before this instant.
    #[serde(default)]
    pub end: Option<chrono::DateTime<chrono::Utc>>,
    /// Only visits with an open missing- or stale-output finding.
    #[serde(default)]
    pub only_findings: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EventAuditRequest {
    /// Audit every event at this site. Omit both fields to audit every site.
    #[serde(default)]
    pub site_id: Option<Uuid>,
    /// Audit one event.
    #[serde(default)]
    pub collection_event_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct VisitsQuery {
    #[serde(default)]
    pub start: Option<DateTime<Utc>>,
    #[serde(default)]
    pub end: Option<DateTime<Utc>>,
    /// 1-based page. Absent, with `page_size` absent, lists every visit.
    #[serde(default)]
    pub page: Option<u64>,
    /// Rows per page, max 200. Absent, with `page` absent, lists every visit.
    #[serde(default)]
    pub page_size: Option<u64>,
    /// `json` (default) or `csv`: the grid as displayed, one column per expected parameter
    /// headed by its code.
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VisitRow {
    pub id: Uuid,
    pub collected_at: DateTime<Utc>,
    /// 'manual' | 'portal_sync'.
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub created_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub notes: Option<String>,
    /// Parameters with at least one non-withdrawn reading at this visit.
    pub parameters_filled: i64,
    /// Open event-audit findings at this visit.
    pub findings_open: i64,
    /// The visit's recompute state: `current` | `queued` | `running` | `failed` | `stale`.
    pub recompute: String,
    /// One cell per parameter measured at the visit (the wide portal row).
    pub cells: Vec<VisitCell>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VisitCell {
    pub parameter_id: Uuid,
    /// The served value: sample mean, else the lowest live replicate.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub value: Option<f64>,
    /// Every replicate in the group is flagged.
    pub flagged: bool,
    /// Every replicate in the group is withdrawn.
    pub withdrawn: bool,
    /// Replicates stored, flagged and withdrawn. A partly curated group serves a mean the
    /// exclusions moved, so the counts are what says a value stepped because replicates were
    /// removed rather than because the measurement changed.
    pub n_total: i64,
    pub n_flagged: i64,
    pub n_withdrawn: i64,
    /// The group's statistics, so a triplicate and a single measurement do not render identically.
    /// `n` counts what the mean stands on, which is `n_total` less the exclusions.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub n: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub stdev: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub median: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub max: Option<f64>,
    /// Which divisor produced `stdev`, and what chose it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub sd_estimator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub sd_estimator_source: Option<String>,
    /// Kind of the oldest open finding on this cell, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub finding: Option<String>,
    /// How many open findings the cell carries, when more than one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub finding_count: Option<i64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExpectedParameter {
    pub parameter_id: Uuid,
    pub code: String,
    pub name: String,
    /// The unit the column's numbers are in, from the site's slot when it declares one and the
    /// catalog default otherwise. A grid of bare numbers cannot be read without it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub units: Option<String>,
    /// `site_parameters.decimal_places` for the slot, null when it declares none.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub decimal_places: Option<i16>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VisitsResponse {
    pub site_id: Uuid,
    pub total: u64,
    pub page: u64,
    pub page_size: u64,
    /// The grid's column set, ordered by code: every parameter this site has sampled, plus its
    /// active configured slots, so a parameter whose readings never formed a `samples` row still
    /// has a column to render into.
    pub expected_parameters: Vec<ExpectedParameter>,
    pub visits: Vec<VisitRow>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct VisitListQuery {
    /// Confine to one site.
    #[serde(default)]
    pub site_id: Option<Uuid>,
    #[serde(default)]
    pub start: Option<DateTime<Utc>>,
    #[serde(default)]
    pub end: Option<DateTime<Utc>>,
    /// 1-based page, default 1.
    #[serde(default)]
    pub page: Option<u64>,
    /// Rows per page, default 100, max 200.
    #[serde(default)]
    pub page_size: Option<u64>,
    /// `collected_at` (default), `parameters_filled`, `findings_open` or `site_name`.
    #[serde(default)]
    pub sort: Option<String>,
    /// `asc` or `desc` (default).
    #[serde(default)]
    pub order: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VisitListRow {
    pub id: Uuid,
    pub site_id: Uuid,
    pub site_name: String,
    pub collected_at: DateTime<Utc>,
    /// 'manual' | 'portal_sync'.
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub created_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub notes: Option<String>,
    /// Parameters with at least one non-withdrawn reading at this visit.
    pub parameters_filled: i64,
    /// Open findings at this visit.
    pub findings_open: i64,
    /// The visit's recompute state: `current` | `queued` | `running` | `failed` | `stale`.
    pub recompute: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EventDetailResponse {
    pub id: Uuid,
    pub site_id: Uuid,
    pub collected_at: DateTime<Utc>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub created_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub notes: Option<String>,
    /// The visit's recompute state: `current` | `queued` | `running` | `failed` | `stale`.
    pub recompute: String,
    pub cells: Vec<EventCell>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EventCell {
    pub parameter_id: Uuid,
    pub parameter_code: String,
    pub parameter_name: String,
    pub stream_id: Uuid,
    /// Which feed these replicates came in on. Two streams can serve one slot at one instant, and
    /// then the grid shows two rows under one parameter name with nothing distinguishing them.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub source_system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub source_key: Option<String>,
    /// How these readings reached the store: `manual`, `csv`, `api` or `sync`. A measurement
    /// without a tool-run blob is not therefore hand-entered; it may be an import or a batch, and
    /// the two are different answers to "did a person type this".
    pub origin: String,
    /// A server-built tool-run blob is stored on the measurement.
    pub has_provenance: bool,
    /// Where the value came from, as the row records it: `tool_run` | `chain` | `csv_import` |
    /// `manual` | `batch` | `sync` | `derived`. Narrower than `origin`, which reads
    /// the stream alone and cannot tell a hand entry from a tool save on the same channel.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub provenance_kind: Option<String>,
    /// The blob's tool name, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub tool: Option<String>,
    /// The value serving arm reports: sample mean, else the lowest unflagged live replicate.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub served_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub sample: Option<CellSample>,
    pub replicates: Vec<CellReplicate>,
    /// The instant's assembled record for this stream, the same shape `/readings/provenance`
    /// serves, so the point record opened from the grid needs no second fetch.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub record: Option<crate::routes::private::readings::models::ProvenanceRecord>,
    /// The oldest open event-audit finding for this cell.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub finding: Option<CellFinding>,
    /// The calculations that read this parameter, by tool name. A person typing into a field needs
    /// to see which script it feeds while typing it, not after the save.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub read_by: Vec<String>,
    /// The calculation that writes this parameter, when one does. Its value is a computed output,
    /// not a measurement, and editing it is a different act from editing an input.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub written_by: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CellSample {
    pub sample_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub mean: Option<f64>,
    /// The sd under the divisor the slot declares. `sd_estimator` names which that is; the other
    /// travels beside it so a reviewer can read both without declaring anything first.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub stdev: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub stdev_sample: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub stdev_population: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub median: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub max: Option<f64>,
    pub n: i32,
    /// 'sample' | 'population', and what chose it ('default' is the fallback having applied).
    pub sd_estimator: String,
    pub sd_estimator_source: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CellReplicate {
    pub replicate_index: i16,
    pub raw_value: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub calibrated_value: Option<f64>,
    pub flagged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub flag_reason: Option<String>,
    pub withdrawn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub withdrawn_at: Option<DateTime<Utc>>,
    /// The base calibration this replicate was corrected with, null when none was.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub calibration_id: Option<Uuid>,
    /// The standard curve applied on top of the base calibration, null when none was.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub standard_curve_id: Option<Uuid>,
    /// The instrument the replicate names. The grid offers it back as the row's declaration, so
    /// re-entering a value does not silently re-attribute it to whatever the slot declares now.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub sensor_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CellFinding {
    pub id: Uuid,
    /// `missing_output`, `stale_output` or `skipped_output`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub tool: Option<String>,
    pub status: String,
}
