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
    /// The caller who created the row, stamped from the request; an update naming it is refused.
    #[crudcrate(exclude(create), on_create = crate::common::actor::current().unwrap_or_default())]
    pub created_by: Option<String>,
    pub notes: Option<String>,
    /// The field day itself is pending: staged by an intern, and nobody has ruled on whether it
    /// should exist (Q177). Set at staging from the stager's level and cleared by the manager's
    /// verify; verifying the visit verifies none of its measurements.
    #[crudcrate(filterable, exclude(create, update))]
    pub unverified: bool,
    /// When the visit was rejected. Nothing deletes a visit: its readings are withdrawn beside it.
    #[crudcrate(exclude(create, update), sortable)]
    pub withdrawn_at: Option<chrono::DateTime<chrono::Utc>>,
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

/// The cells an operator has typed at a visit and not saved. A cell the request leaves out is one
/// nobody touched, and reads from the store.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PreviewEventRequest {
    #[serde(default)]
    pub staged: Vec<crate::routes::private::tools::staged::StagedCell>,
}

/// A preview at a site and instant no visit stands at yet: the typed cells of a new row.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PreviewUnstagedRequest {
    pub site_id: Uuid,
    pub collected_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub staged: Vec<crate::routes::private::tools::staged::StagedCell>,
}

/// One visit of a field day: a site and the instant it was sampled.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StageVisitRow {
    pub site_id: Uuid,
    pub collected_at: chrono::DateTime<chrono::Utc>,
}

/// A field day: the visits it covers, each at its own site and instant.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StageEventsRequest {
    pub visits: Vec<StageVisitRow>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StagedEvent {
    pub id: Uuid,
    pub site_id: Uuid,
    pub collected_at: chrono::DateTime<chrono::Utc>,
    pub source: String,
    #[schema(required)]
    pub created_by: Option<String>,
    #[schema(required)]
    pub notes: Option<String>,
    /// The field day is pending a manager's ruling. A visit already standing keeps the state it
    /// had, so staging into someone else's verified visit does not reopen it.
    pub unverified: bool,
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
    /// Hold the findings to the ones one calculation raised. A narrowing, so it needs a scope
    /// beside it.
    #[serde(default)]
    pub calculation: Option<String>,
    /// Every visit a script version produced values at, as the stored provenance names it. A
    /// scope of its own: this is what an author's migrate arm asks for.
    #[serde(default)]
    pub version: Option<Uuid>,
    /// Only visits whose stored provenance names this constant. A scope in its own right.
    #[serde(default)]
    pub constant: Option<String>,
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
    /// The field day is pending a manager's ruling (Q177). Its measurements cannot be verified
    /// until it is.
    pub unverified: bool,
    /// When the visit was rejected, its readings withdrawn beside it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub withdrawn_at: Option<DateTime<Utc>>,
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
    /// Replicates entered but not yet verified. The sample trigger counts none of them, so a
    /// cell whose replicates are all pending serves `n = 0` beside values that are on screen.
    pub n_unverified: i64,
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
    /// Kind of the oldest open finding on this cell, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub finding: Option<String>,
    /// How many open findings the cell carries, when more than one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub finding_count: Option<i64>,
    /// Every stored replicate, in index order, singletons included. The listing carries them so a
    /// parameter's column opens to its repeats from what the grid already holds (Q200).
    pub replicates: Vec<VisitReplicate>,
    /// A tool run stands behind this measurement. Q8 sends such a value back into its tool to be
    /// corrected, and a value with none through the edit primitive, so the grid needs to know
    /// which arm a cell takes before it offers an edit.
    pub has_provenance: bool,
    /// The blob's tool name, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub tool: Option<String>,
    /// The run behind the value, when a tool computed it. The trace endpoint replays it, which is
    /// what lets a cell show the formula rather than only the name of what ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub tool_run_id: Option<Uuid>,
    /// Each distinct standard curve the group's replicates were corrected through, in replicate
    /// order (Q97). Empty for a value no curve corrected.
    pub curves: Vec<VisitCellCurve>,
}

/// A curve a listing's cell was corrected through: what the grid names it by.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct VisitCellCurve {
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub name: Option<String>,
}

/// One replicate of a listing's cell: enough to draw the expanded column and to seed the edit
/// model, and no more. The curves, instruments and flag reasons behind it stay on
/// `/collection_events/{id}/detail`, which is what the point record reads.
#[derive(Debug, Serialize, ToSchema)]
pub struct VisitReplicate {
    pub replicate_index: i16,
    /// The feed this replicate came in on. With the instant and the index it is the key a
    /// correction is made against, so it sits on the replicate rather than on the cell: two
    /// streams can serve one slot and a correction must name the right one.
    pub stream_id: Uuid,
    /// The corrected value where the reading carries one, else the raw value.
    pub value: f64,
    /// The measurement before any curve: what an entry into this replicate's group sends back
    /// for it, since the save corrects what it is sent.
    pub raw_value: f64,
    /// The windowed calibration that corrected it, if one did.
    #[schema(required)]
    pub calibration_id: Option<Uuid>,
    /// The standard curve chosen for it, if one was.
    #[schema(required)]
    pub standard_curve_id: Option<Uuid>,
    pub flagged: bool,
    pub withdrawn: bool,
    /// A pending entry: stored and shown, counted by no statistic and served nowhere.
    pub unverified: bool,
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
    /// The calculation that writes this parameter, when one does. The column's values are computed
    /// outputs, so the grid reads them and sends a correction back through the calculation (Q8)
    /// rather than offering a keystroke.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub written_by: Option<String>,
    /// The calculations that read this parameter, by tool name, so every cell of the column says
    /// what a typed value feeds whichever visit it is typed at.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub read_by: Vec<String>,
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
    /// The field day is pending a manager's ruling (Q177). Its measurements cannot be verified
    /// until it is.
    pub unverified: bool,
    /// When the visit was rejected, its readings withdrawn beside it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub withdrawn_at: Option<DateTime<Utc>>,
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
    /// The field day is pending a manager's ruling (Q177). Its measurements cannot be verified
    /// until it is.
    pub unverified: bool,
    /// When the visit was rejected, its readings withdrawn beside it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub withdrawn_at: Option<DateTime<Utc>>,
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
    /// The run behind the value, when a tool computed it. The trace endpoint replays it, which is
    /// what lets a cell show the formula rather than only the name of what ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub tool_run_id: Option<Uuid>,
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
    /// The oldest open finding for this cell, slot-keyed or placed through its stream's pairing.
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
    /// The sample standard deviation (n-1).
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
    pub n: i32,
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
    /// A pending entry: stored and shown, counted by no statistic and served nowhere (Q18, Q21).
    pub unverified: bool,
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
    /// What that instrument is: `device` | `lab` | `source_parameter` | `entry_channel`. The two
    /// bookkeeping kinds record that nothing was declared, so a write may not name one and the
    /// grid does not offer one back as a declaration.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub sensor_kind: Option<String>,
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
