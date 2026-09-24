//! The sites entity, and every request and response shape the site-scoped endpoints publish.

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset, Utc};
use crudcrate::EntityToModels;
use sea_orm::FromQueryResult;
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::service::SiteOperations;
use crate::routes::private::meteoswiss::models::ExternalSource;

// --- The sites entity ---

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "sites")]
#[crudcrate(
    api_struct = "Site",
    name_singular = "site",
    name_plural = "sites",
    generate_router,
    operations = SiteOperations
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable, sortable)]
    pub project_id: Option<Uuid>,
    // Optional on the wire, mandatory in practice: a DB trigger derives the project's default
    // subproject when a site is created/moved without one, and keeps `project_id` in sync with it.
    #[crudcrate(filterable, sortable)]
    pub subproject_id: Option<Uuid>,
    #[sea_orm(unique)]
    #[crudcrate(filterable, fulltext, sortable)]
    pub name: String,
    #[crudcrate(sortable)]
    pub latitude: Option<f64>,
    #[crudcrate(sortable)]
    pub longitude: Option<f64>,
    #[crudcrate(sortable)]
    pub altitude_m: Option<f64>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Stamped by every sync path that mints a site, and by nothing else, so it is what says a
    /// row arrived from a source rather than by hand.
    #[crudcrate(exclude(create, update), filterable, sortable)]
    pub discovered_at: Option<chrono::DateTime<chrono::Utc>>,
    pub public_code: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::projects::Entity",
        from = "Column::ProjectId",
        to = "crate::routes::private::projects::Column::Id"
    )]
    Project,
    #[sea_orm(
        belongs_to = "crate::routes::private::projects::subprojects::Entity",
        from = "Column::SubprojectId",
        to = "crate::routes::private::projects::subprojects::Column::Id"
    )]
    Subproject,
    #[sea_orm(has_many = "crate::routes::private::site_parameters::Entity")]
    SiteParameters,
    #[sea_orm(has_many = "crate::routes::private::sensor_deployments::Entity")]
    SensorDeployments,
    #[sea_orm(has_many = "crate::routes::private::readings::Entity")]
    Readings,
}

impl Related<crate::routes::private::projects::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Project.def()
    }
}

impl Related<crate::routes::private::projects::subprojects::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Subproject.def()
    }
}

impl Related<crate::routes::private::site_parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SiteParameters.def()
    }
}

impl Related<crate::routes::private::sensor_deployments::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SensorDeployments.def()
    }
}

impl Related<crate::routes::private::readings::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Readings.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

// --- Shared references and the site projections ---

/// Brief project reference for embedding in responses
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ProjectRef {
    pub id: Uuid,
    pub name: String,
}

/// Brief site reference for embedding in responses
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SiteRef {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Serialize, ToSchema)]
#[schema(as = SiteResponse)]
pub struct SiteProjection {
    pub id: Uuid,
    #[schema(required)]
    pub project_id: Option<Uuid>,
    #[schema(required)]
    pub subproject_id: Option<Uuid>,
    pub name: String,
    #[schema(required)]
    pub latitude: Option<f64>,
    #[schema(required)]
    pub longitude: Option<f64>,
    #[schema(required)]
    pub altitude_m: Option<f64>,
}

/// Parameter information embedded in site responses
#[derive(Debug, Serialize, ToSchema)]
pub struct ParameterResponse {
    /// Site-parameter id
    pub id: Uuid,
    /// Global catalog parameter id
    pub parameter_id: Uuid,
    /// Stable parameter code (catalog `code`, e.g. "DOmgL")
    pub code: String,
    /// Human-readable parameter name (catalog `name`, e.g. "Dissolved Oxygen")
    pub name: String,
    /// Units, from the catalog `default_units`
    #[schema(required)]
    pub units: Option<String>,
    /// How this site fills the slot: 'manual' or 'tool'
    pub entry_mode: String,
    pub sensor_type: String,
    /// Display precision the client formats with; the API serves full precision.
    #[schema(required)]
    pub decimal_places: Option<i16>,
    #[schema(required)]
    pub sample_interval_sec: Option<i32>,
    #[schema(required)]
    pub is_active: Option<bool>,
    /// Earliest reading timestamp for this parameter at the site
    #[schema(required)]
    pub data_start: Option<DateTime<Utc>>,
    /// Latest reading timestamp for this parameter at the site
    #[schema(required)]
    pub data_end: Option<DateTime<Utc>>,
    /// Number of readings for this parameter at the site
    #[schema(required)]
    pub reading_count: Option<i64>,
    /// Whether any continuous (or legacy untagged) readings exist for this parameter at the site
    pub has_continuous: bool,
    /// Whether any spot (grab/lab) readings exist for this parameter at the site
    pub has_spot: bool,
    /// The slot's declared cadence: 'high' (a stream carries it) or 'low' (a person records it at
    /// a visit). Low-frequency series render marker-only over their full range and skip the
    /// aggregate path.
    pub frequency: String,
    /// The outside feed the values come from, where the site subscribes to one for this parameter.
    /// None is the site's own instruments.
    #[schema(required)]
    pub external_source: Option<ExternalSource>,
}

/// Detailed site response with project info, parameters, and data range
#[derive(Debug, Serialize, ToSchema)]
pub struct SiteDetailResponse {
    pub id: Uuid,
    pub name: String,
    #[schema(required)]
    pub latitude: Option<f64>,
    #[schema(required)]
    pub longitude: Option<f64>,
    #[schema(required)]
    pub altitude_m: Option<f64>,
    #[schema(required)]
    pub project: Option<ProjectRef>,
    pub parameters: Vec<ParameterResponse>,
    /// Earliest reading timestamp for this site
    #[schema(required)]
    pub data_start: Option<DateTime<Utc>>,
    /// Latest reading timestamp for this site
    #[schema(required)]
    pub data_end: Option<DateTime<Utc>>,
    /// Total number of readings for this site
    pub reading_count: i64,
}

// --- Site detail and the parameter list ---

#[derive(Debug, FromQueryResult)]
pub(super) struct ContinuousExtentRow {
    pub(super) parameter_id: Uuid,
    pub(super) min_bucket: Option<DateTime<Utc>>,
    pub(super) max_bucket: Option<DateTime<Utc>>,
    pub(super) count: i64,
}

#[derive(Debug, FromQueryResult)]
pub(super) struct SpotExtentRow {
    pub(super) parameter_id: Uuid,
    pub(super) min_time: Option<DateTime<Utc>>,
    pub(super) max_time: Option<DateTime<Utc>>,
    pub(super) count: i64,
}

#[derive(Debug, FromQueryResult)]
pub(super) struct RecentExtentRow {
    pub(super) parameter_id: Uuid,
    pub(super) min_time: Option<DateTime<Utc>>,
    pub(super) max_time: Option<DateTime<Utc>>,
    pub(super) spot_count: i64,
    pub(super) continuous_count: i64,
}

#[derive(Debug, FromQueryResult)]
pub(super) struct FlaggedHeadRow {
    pub(super) parameter_id: Uuid,
    pub(super) min_time: Option<DateTime<Utc>>,
}

#[derive(Debug, FromQueryResult)]
pub(super) struct CursorRow {
    pub(super) parameter_id: Uuid,
    pub(super) max_time: Option<DateTime<Utc>>,
}

// --- Readings ---

/// One reading as the series query returns it. `severity` is NULL unless `alarms=true`.
#[derive(Debug, FromQueryResult)]
pub(super) struct ReadingRow {
    pub(super) parameter_id: Uuid,
    pub(super) time: chrono::DateTime<chrono::FixedOffset>,
    /// NULL on the collapsed view, which serves one row per instant. On the replicate view it is
    /// half the row's key: replicates share a timestamp, so time alone does not identify a row.
    pub(super) replicate_index: Option<i16>,
    pub(super) stream_id: Uuid,
    pub(super) value: f64,
    pub(super) severity: Option<i16>,
    pub(super) is_flagged: Option<bool>,
    pub(super) flag_reason: Option<String>,
    pub(super) measurement_type: Option<String>,
    pub(super) sample_id: Option<Uuid>,
    pub(super) calibration_id: Option<Uuid>,
    pub(super) standard_curve_id: Option<Uuid>,
    /// Only selected under `include_withdrawn`; false everywhere else, since nothing else serves
    /// a retracted row.
    pub(super) withdrawn: Option<bool>,
    /// Entered by someone whose entries need countersigning. Always selected: the public arm, the
    /// alarms and the seasonal check all exclude such a row, so the private arm has to say it is
    /// there or nothing shows a manager what is waiting.
    pub(super) unverified: Option<bool>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReadingsResponse {
    /// Project this data belongs to
    #[schema(required)]
    pub project: Option<ProjectRef>,
    /// Site this data belongs to
    pub site: SiteRef,
    /// Start of time range (null if no data)
    #[schema(required)]
    pub start: Option<DateTime<Utc>>,
    /// End of time range (null if no data)
    #[schema(required)]
    pub end: Option<DateTime<Utc>>,
    /// Array of timestamps (aligned to 10-minute intervals)
    pub times: Vec<DateTime<Utc>>,
    /// The replicate index of each row, present only under `include_replicates`. Replicates share
    /// a timestamp, so `times` alone does not identify a row there.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub replicate_indices: Option<Vec<i16>>,
    /// Array of parameters with their values
    pub parameters: Vec<ParameterData>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ParameterData {
    pub id: Uuid,
    /// Global parameter id (the catalog parameter this site_parameter references)
    pub parameter_id: Uuid,
    /// Stable parameter code (catalog `code`), used as the CSV/NDJSON column key
    pub code: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub display_name: Option<String>,
    #[serde(rename = "type")]
    pub sensor_type: String,
    #[schema(required)]
    pub units: Option<String>,
    /// `site_parameters.decimal_places` for the slot, null when it declares none. The private arm
    /// serves values as stored; this is what a consumer needs to render them at the declared
    /// precision without asking a second endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub decimal_places: Option<i16>,
    /// Values array (same length as times, null for missing data)
    pub values: Vec<Option<f64>>,
    /// Severity levels (0=ok, 1=warning, 2=alarm). Only present when alarms=true.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub severities: Option<Vec<Option<i16>>>,
    /// Boolean flags marking outliers (same length as times). Only present when `include_flagged=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub flagged: Option<Vec<Option<bool>>>,
    /// Reasons for flagging (same length as times). Only present when `include_flagged=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub flag_reasons: Option<Vec<Option<String>>>,
    /// Per-point measurement type (continuous/spot/derived). Only present when `include_measurement_type=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub measurement_types: Option<Vec<Option<String>>>,
    /// Per-point base calibration reference (same length as times). Only present when
    /// `include_curves=true`. Null where no calibration was applied, which is what distinguishes an
    /// unrecorded base from an identity one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub calibration_ids: Option<Vec<Option<Uuid>>>,
    /// Per-point standard curve reference, applied after the base calibration (same length as
    /// times). Only present when `include_curves=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub standard_curve_ids: Option<Vec<Option<Uuid>>>,
    /// Per-point sample stats with individual replicates (same length as times; null where the
    /// point is not a replicate group). Only present when `include_sample_stats=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub samples: Option<Vec<Option<SampleStatOut>>>,
    /// The streams paired into this slot (series-level; per-point exactness is
    /// `/readings/provenance`'s job). Only present when `include_origin=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub origins: Option<Vec<OriginRef>>,
    /// Whether each served spot point is a retracted instant (same length as times). Only present
    /// when `include_withdrawn=true`, which is also what makes such an instant served at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub withdrawn: Option<Vec<Option<bool>>>,
    /// Whether each served point still needs countersigning (same length as times). Always served,
    /// because it is the one surface that shows it: the public arm, the alarms and the seasonal
    /// check leave such a reading out entirely.
    #[schema(required)]
    pub unverified: Option<Vec<Option<bool>>>,
    /// How many spot instants in the window the source has retracted in full, whether or not they
    /// are served. Present whenever the request covers the spot arm, so a chart can say a visit was
    /// taken back without fetching the points.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub withdrawn_count: Option<i64>,
}

/// One ingestion channel serving a slot.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OriginRef {
    pub stream_id: Uuid,
    pub source_system: String,
    pub source_key: String,
}

/// One replicate behind a grab-sample point.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ReplicateOut {
    pub replicate_index: i16,
    pub raw_value: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub calibrated_value: Option<f64>,
    /// The base calibration this replicate was corrected with, null when none was.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub calibration_id: Option<Uuid>,
    /// The standard curve applied on top of the base calibration, null when none was.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub standard_curve_id: Option<Uuid>,
    pub flagged: bool,
    /// The source's claimed window no longer contains this replicate. It is excluded from the
    /// sample statistics served alongside it, so a consumer listing the replicates under `n` has to
    /// say so or it prints more values than it counts.
    pub withdrawn: bool,
}

/// Sample statistics and replicate values behind one served grab point.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SampleStatOut {
    pub sample_id: Uuid,
    pub n: i32,
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
    pub replicates: Vec<ReplicateOut>,
}

#[derive(Debug, Deserialize, Serialize, IntoParams)]
pub struct SiteReadingsQuery {
    /// Start time (optional, ISO 8601). If omitted, the window opens
    /// `DEFAULT_READINGS_LOOKBACK_DAYS` before now (7 days unless the deployment sets it).
    pub start: Option<DateTime<Utc>>,
    /// End time (optional, ISO 8601). If omitted, returns to latest data.
    pub end: Option<DateTime<Utc>>,
    /// Filter by sensor types (comma-separated)
    pub sensor_types: Option<String>,
    /// Filter to a specific subset of parameters (comma-separated UUIDs). If omitted, returns all parameters configured for the site.
    pub parameter_ids: Option<String>,
    /// Response format: json (default), ndjson, csv
    #[serde(default = "crate::common::bulk::default_format")]
    pub format: String,
    /// Include alarm severity data (threshold violations)
    pub alarms: Option<bool>,
    /// Filter by measurement type: continuous, spot, derived. Omit to return all types mixed.
    pub measurement_type: Option<String>,
    /// Include flagged readings with flag metadata (default: true). When false, excludes flagged readings entirely.
    pub include_flagged: Option<bool>,
    /// Add the per-point `<code>_flagged` and `<code>_flag_reason` columns to the CSV/NDJSON
    /// export. JSON carries the flag arrays whenever `include_flagged` is on, which is the
    /// default, so the export columns need their own opt-in to keep the default header stable.
    pub include_flags: Option<bool>,
    /// Include replicate readings (default: false). When false, a replicate group is returned as
    /// one row at its lowest unflagged replicate index, carrying the group's sample mean.
    /// Refused together with `include_sample_stats`: a row per replicate and a row per instant are
    /// two files, not one.
    pub include_replicates: Option<bool>,
    /// Filter by sample ID to retrieve replicates for a specific sample.
    pub sample_id: Option<Uuid>,
    /// Include a per-point measurement_type indicator (continuous/spot/derived) on each parameter.
    pub include_measurement_type: Option<bool>,
    /// Add the per-point `calibration_id` and `standard_curve_id` references, so a served value
    /// states which curves produced it.
    pub include_curves: Option<bool>,
    /// Attach per-point sample statistics (n, mean, stdev, min, max) and the individual
    /// replicate values behind each grab point. Spot data only; one batched lookup.
    /// Refused together with `include_replicates`.
    pub include_sample_stats: Option<bool>,
    /// Attach each parameter's ingestion origins (the streams paired into the slot).
    pub include_origin: Option<bool>,
    /// Include rows the source has retracted. Replicate exports exclude them by default, as every
    /// other serving path does; with this on they are exported and carry a `withdrawn` column.
    pub include_withdrawn: Option<bool>,
}

/// Everything that shapes a readings body. The query is flattened in whole, so a field added to
/// `SiteReadingsQuery` enters the key without anyone remembering to list it.
#[derive(Serialize)]
pub(super) struct ReadingsCacheKey<'a> {
    pub(super) effective_start: DateTime<Utc>,
    pub(super) effective_end: Option<DateTime<Utc>>,
    pub(super) resolved_format: &'a str,
    #[serde(flatten)]
    pub(super) query: &'a SiteReadingsQuery,
}

/// One replicate of a spot group, as the statistics query returns it. `is_flagged` is the only
/// nullable column: nothing has flagged the row yet, which reads as not flagged.
#[derive(sea_orm::FromQueryResult)]
pub(super) struct SampleReplicateRow {
    pub(super) sample_id: Uuid,
    pub(super) replicate_index: i16,
    pub(super) raw_value: f64,
    pub(super) calibrated_value: Option<f64>,
    pub(super) calibration_id: Option<Uuid>,
    pub(super) standard_curve_id: Option<Uuid>,
    pub(super) is_flagged: Option<bool>,
    pub(super) withdrawn: bool,
}

/// Spot instants in the window with no live replicate left, per parameter.
///
/// An instant every one of whose rows carries `withdrawn_at` is served by nothing, so a chart drawn
/// from the readings response alone cannot tell it from a visit never made. This is what lets it
/// say so.
/// One parameter's withdrawn-row count over the window.
#[derive(FromQueryResult)]
pub(super) struct WithdrawnCount {
    pub(super) parameter_id: Uuid,
    pub(super) withdrawn_count: i64,
}

// --- Aggregates ---

#[derive(Debug, Serialize, ToSchema)]
pub struct AggregatesResponse {
    /// Project this data belongs to
    #[schema(required)]
    pub project: Option<ProjectRef>,
    /// Site this data belongs to
    pub site: SiteRef,
    /// Aggregation resolution
    pub resolution: String,
    /// Start of time range
    pub start: DateTime<Utc>,
    /// End of time range
    pub end: DateTime<Utc>,
    /// Array of bucket timestamps
    pub times: Vec<DateTime<Utc>>,
    /// Array of parameters with their aggregated values
    pub parameters: Vec<ParameterAggregateData>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ParameterAggregateData {
    pub id: Uuid,
    /// Global parameter id (the catalog parameter this site_parameter references)
    pub parameter_id: Uuid,
    /// Owning sensor for this series. Only present when `split_by_sensor=true` (null = the
    /// unattributed/legacy group). Absent in the default collapsed response.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub sensor_id: Option<Uuid>,
    /// Stable parameter code (catalog `code`), used as the CSV/NDJSON column key
    pub code: String,
    pub name: String,
    #[serde(rename = "type")]
    pub sensor_type: String,
    #[schema(required)]
    pub units: Option<String>,
    /// Average values array (same length as times)
    pub avg: Vec<Option<f64>>,
    /// Minimum values array
    pub min: Vec<Option<f64>>,
    /// Maximum values array
    pub max: Vec<Option<f64>>,
    /// Count of readings per bucket
    pub count: Vec<i64>,
    /// Maximum severity level per bucket (0=ok, 1=warning, 2=alarm). Only present when alarms=true.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub max_severity: Option<Vec<Option<i16>>>,
    /// Count of flagged readings per bucket (always present).
    pub flagged_count: Vec<i64>,
}

/// One rollup bucket. `sensor_id` is NULL on the collapsed read, which selects it as a literal so
/// both reads share this row shape.
#[derive(Debug, FromQueryResult)]
pub(super) struct AggregateRow {
    pub(super) bucket: DateTime<Utc>,
    pub(super) parameter_id: Uuid,
    pub(super) sensor_id: Option<Uuid>,
    pub(super) avg_value: Option<f64>,
    pub(super) min_value: Option<f64>,
    pub(super) max_value: Option<f64>,
    pub(super) count: i64,
}

#[derive(Debug, FromQueryResult)]
pub(super) struct FlaggedBucketRow {
    pub(super) bucket: DateTime<Utc>,
    pub(super) parameter_id: Uuid,
    pub(super) sensor_id: Option<Uuid>,
    pub(super) flagged_count: i64,
}

/// A series key: the slot, plus the sensor when the sensor dimension is kept.
pub(super) type SeriesKey = (Uuid, Option<Uuid>);

pub(super) type AggTuple = (Option<f64>, Option<f64>, Option<f64>, i64);

#[derive(Debug, Deserialize, Serialize, IntoParams)]
pub struct SiteAggregatesQuery {
    /// Start time (required, ISO 8601)
    pub start: DateTime<Utc>,
    /// End time (required, ISO 8601)
    pub end: DateTime<Utc>,
    /// Filter by sensor types (comma-separated)
    pub sensor_types: Option<String>,
    /// Response format: json (default), ndjson, csv
    #[serde(default = "crate::common::bulk::default_format")]
    pub format: String,
    /// Include alarm severity data (threshold violations)
    pub alarms: Option<bool>,
    /// Return one series per sensor instead of collapsing the sensor dimension. JSON only; each
    /// returned parameter entry carries its `sensor_id` (null = the unattributed group).
    pub split_by_sensor: Option<bool>,
}

/// Everything that shapes an aggregates body, with the query flattened in whole so a new field
/// enters the key by construction.
#[derive(Serialize)]
pub(super) struct AggregatesCacheKey<'a> {
    pub(super) resolution: &'a str,
    pub(super) resolved_format: &'a str,
    /// The split as applied, which is off for the bulk formats whatever the query said.
    pub(super) effective_split: bool,
    #[serde(flatten)]
    pub(super) query: &'a SiteAggregatesQuery,
}

// --- Status events ---

#[derive(Debug, Deserialize, IntoParams)]
pub struct StatusEventsQuery {
    /// Start time (optional, ISO 8601). If omitted, returns from earliest data.
    pub start: Option<DateTime<Utc>>,
    /// End time (optional, ISO 8601). If omitted, returns to latest data.
    pub end: Option<DateTime<Utc>>,
    /// Response format: json (default), ndjson, csv
    #[serde(default = "crate::common::bulk::default_format")]
    pub format: String,
    /// Max events to return (JSON only, capped at 1000). If omitted, returns all
    /// matching events. CSV/NDJSON always export the full range.
    pub limit: Option<u64>,
    /// Pagination offset (JSON only, default 0).
    pub offset: Option<u64>,
    /// Sort by time: `asc` (default) or `desc`.
    pub order: Option<String>,
}

/// A single status event in the JSON response
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StatusEventData {
    pub parameter_id: Uuid,
    pub time: DateTime<Utc>,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub sensor_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StatusEventsResponse {
    /// Site this data belongs to
    pub site: SiteRef,
    /// Array of status events
    pub events: Vec<StatusEventData>,
    /// Total events matching the time range (ignores limit/offset)
    pub total: u64,
}

// --- Annotations and the export summary ---

#[derive(Debug, Deserialize, IntoParams)]
pub struct SiteAnnotationsQuery {
    /// Filter by parameter UUID
    pub parameter_id: Option<Uuid>,
    /// Filter by a comma-separated list of parameter UUIDs
    pub parameter_ids: Option<String>,
    /// Start time (ISO 8601)
    pub start: Option<DateTime<Utc>>,
    /// End time (ISO 8601)
    pub end: Option<DateTime<Utc>>,
    /// Response format: json (default) or csv
    pub format: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct ExportSummaryQuery {
    /// Start time (ISO 8601)
    pub start: DateTime<Utc>,
    /// End time (ISO 8601)
    pub end: DateTime<Utc>,
}

#[derive(Debug, Default, Serialize, ToSchema)]
pub struct ParameterExportSummary {
    pub parameter_id: Uuid,
    pub code: String,
    pub annotation_count: i64,
    /// Distinct served instants of the parameter inside both the query range and an
    /// annotation's own range.
    pub annotated_points: i64,
    pub flagged_readings: i64,
    /// Served readings beyond the first replicate slot (replicate_index > 0).
    pub replicate_readings: i64,
    /// Readings breaching a warning or alarm bound over the range.
    pub alarm_readings: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExportSummaryResponse {
    pub annotation_count: i64,
    pub annotated_points: i64,
    pub flagged_readings: i64,
    pub replicate_readings: i64,
    pub alarm_readings: i64,
    pub per_parameter: Vec<ParameterExportSummary>,
}

/// What an export of this site and range can carry beyond the plain series: annotation, flagged,
/// replicate and alarm counts, per parameter. The export dialog enables each option from these
/// numbers and shows them beside it.
/// Per-parameter annotation counts over the export's range.
#[derive(FromQueryResult)]
pub(super) struct AnnotationCounts {
    pub(super) pid: Uuid,
    pub(super) ann_count: i64,
    pub(super) pts: i64,
}

/// Per-parameter flagged and extra-replicate counts over the same range.
#[derive(FromQueryResult)]
pub(super) struct CurationCounts {
    pub(super) pid: Uuid,
    pub(super) flagged: i64,
    pub(super) reps: i64,
}

// --- Statistics ---

#[derive(Debug, Deserialize, IntoParams)]
pub struct StatisticsQuery {
    /// Start of the range (ISO 8601). Defaults to the configured lookback.
    pub start: Option<DateTime<Utc>>,
    /// End of the range (ISO 8601). Open-ended when omitted.
    pub end: Option<DateTime<Utc>>,
    /// Global parameter ids, comma-separated. Every parameter at the site when omitted.
    pub parameter_ids: Option<String>,
    /// `continuous` (default) or `spot`. A spot period is summarised over the served instant
    /// values, which are sample means, not over the individual replicates.
    pub measurement_type: Option<String>,
}

/// One parameter's row of the summary query. Derived rather than hand-decoded so a column added
/// to the query and not to its reader is a compile error.
#[derive(FromQueryResult)]
pub(super) struct StatisticsRow {
    pub(super) parameter_id: Uuid,
    pub(super) code: String,
    pub(super) name: String,
    pub(super) units: Option<String>,
    pub(super) decimal_places: Option<i16>,
    pub(super) time_points: i64,
    pub(super) n: i64,
    pub(super) median: Option<f64>,
    pub(super) mean: Option<f64>,
    pub(super) stdev_sample: Option<f64>,
    pub(super) min_value: Option<f64>,
    pub(super) max_value: Option<f64>,
}

/// The portal's eight rows for one parameter.
#[derive(Debug, Serialize, ToSchema)]
pub struct ParameterStatistics {
    pub parameter_id: Uuid,
    pub code: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub units: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub decimal_places: Option<i16>,
    /// Instants in the range, whether or not each carries a value.
    pub time_points: i64,
    /// Instants carrying a value: what every statistic below is computed over.
    pub n: i64,
    /// `time_points` less `n`, the portal's NA's row.
    pub nulls: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub median: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub mean: Option<f64>,
    /// The sample standard deviation (n-1), R's `sd()`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub stdev_sample: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub max: Option<f64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StatisticsResponse {
    pub site: SiteRef,
    pub start: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub end: Option<DateTime<Utc>>,
    /// `continuous` or `spot`: which cadence the rows summarise.
    pub measurement_type: String,
    pub parameters: Vec<ParameterStatistics>,
}

// --- Replicate export ---

#[derive(Debug, Deserialize, IntoParams)]
pub struct ReplicatesQuery {
    /// Start of the range (optional, ISO 8601). Defaults to the configured lookback.
    pub start: Option<DateTime<Utc>>,
    /// End of the range (optional, ISO 8601). Open-ended when omitted.
    pub end: Option<DateTime<Utc>>,
    /// Restrict to these global parameter ids (comma-separated).
    pub parameter_ids: Option<String>,
    /// Include the replicates of instants the source has retracted, marked as retracted.
    pub include_withdrawn: Option<bool>,
    /// Response format: json (default) or csv.
    #[serde(default = "crate::common::bulk::default_format")]
    pub format: String,
}

/// One measurement behind a spot instant.
#[derive(Debug, Serialize, ToSchema, FromQueryResult)]
pub struct ReplicateRow {
    pub time: DateTime<FixedOffset>,
    /// The catalog code, which is the column name the site export writes the instant under.
    pub parameter: String,
    /// The instant's sample, `{code}_sample_id` on the site export. Null where the instant holds
    /// one measurement, which forms no sample.
    #[schema(required)]
    pub sample_id: Option<Uuid>,
    pub replicate_index: i16,
    /// Corrected where a curve applied, raw otherwise: the value the statistics were computed from.
    #[schema(required)]
    pub value: Option<f64>,
    pub flagged: bool,
    pub withdrawn: bool,
    #[schema(required)]
    pub source_system: Option<String>,
    #[schema(required)]
    pub source_key: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReplicatesResponse {
    pub site: SiteRef,
    pub rows: Vec<ReplicateRow>,
}

// --- Sensor versus grab ---

fn default_window_start() -> f64 {
    2.0
}

fn default_window_end() -> f64 {
    6.0
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct SensorVsGrabQuery {
    /// Global parameter id to compare (continuous sensor readings vs grab samples).
    pub parameter_id: Uuid,
    /// Start of the grab-sample time range (optional, ISO 8601). Defaults to the configured lookback.
    pub start: Option<DateTime<Utc>>,
    /// End of the grab-sample time range (optional, ISO 8601). Open-ended when omitted.
    pub end: Option<DateTime<Utc>>,
    /// Start of the post-grab averaging window, in hours after each grab (default 2).
    #[serde(default = "default_window_start")]
    pub window_start_hours: f64,
    /// End of the post-grab averaging window, in hours after each grab (default 6).
    #[serde(default = "default_window_end")]
    pub window_end_hours: f64,
    /// Response format: json (default) or csv.
    #[serde(default = "crate::common::bulk::default_format")]
    pub format: String,
}

/// One grab sample paired with the continuous-sensor average over the post-grab window.
#[derive(Debug, Serialize, ToSchema)]
pub struct SensorVsGrabRow {
    /// Grab sample collection time.
    pub time: DateTime<Utc>,
    /// Grab sample value (mean of replicates).
    #[schema(required)]
    pub grab_value: Option<f64>,
    /// Grab sample standard deviation across replicates.
    #[schema(required)]
    pub grab_sd: Option<f64>,
    /// Number of grab replicates.
    pub grab_n: i32,
    /// Mean continuous sensor reading over [time + window_start_hours, time + window_end_hours].
    #[schema(required)]
    pub sensor_avg: Option<f64>,
    /// Standard deviation of continuous sensor readings in the window.
    #[schema(required)]
    pub sensor_sd: Option<f64>,
    /// Number of continuous sensor readings in the window.
    pub sensor_n: i64,
    /// grab_value − sensor_avg (null when either side is missing).
    #[schema(required)]
    pub difference: Option<f64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SensorVsGrabResponse {
    pub site: SiteRef,
    pub parameter_id: Uuid,
    pub window_start_hours: f64,
    pub window_end_hours: f64,
    pub rows: Vec<SensorVsGrabRow>,
}

#[derive(Debug, FromQueryResult)]
pub(super) struct ComparisonRow {
    pub(super) grab_time: DateTime<FixedOffset>,
    pub(super) grab_value: Option<f64>,
    pub(super) grab_sd: Option<f64>,
    pub(super) grab_n: i32,
    pub(super) sensor_avg: Option<f64>,
    pub(super) sensor_sd: Option<f64>,
    pub(super) sensor_n: i64,
}

// --- Sensor identity bands ---

#[derive(Debug, Deserialize, IntoParams)]
pub struct SensorIdentityQuery {
    /// Start time (required, ISO 8601).
    pub start: DateTime<Utc>,
    /// End time (required, ISO 8601).
    pub end: DateTime<Utc>,
    /// Optional comma-separated global parameter UUIDs to restrict to.
    pub parameter_ids: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IdentityBand {
    pub deployment_id: Uuid,
    pub sensor_id: Uuid,
    #[schema(required)]
    pub sensor_serial: Option<String>,
    #[schema(required)]
    pub sensor_name: Option<String>,
    pub site_id: Uuid,
    #[schema(required)]
    pub site_name: Option<String>,
    pub parameter_id: Uuid,
    pub from: DateTime<Utc>,
    #[schema(required)]
    pub until: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CalibrationMarker {
    pub calibration_id: Uuid,
    pub sensor_id: Uuid,
    pub slope: f64,
    pub intercept: f64,
    pub valid_from: DateTime<Utc>,
    #[schema(required)]
    pub valid_until: Option<DateTime<Utc>>,
}

/// Sensor-identity bands + calibration markers for a site over a window, keyed by global
/// `parameter_id`. Drives the chart overlays. Sourced from the deployment/calibration tables so
/// it is correct mid-reprocess.
#[derive(Debug, Serialize, ToSchema)]
pub struct SensorIdentityResponse {
    pub site_id: Uuid,
    pub bands: HashMap<Uuid, Vec<IdentityBand>>,
    pub calibrations: HashMap<Uuid, Vec<CalibrationMarker>>,
}

/// The two window queries this view makes, as their SELECTs return them.
#[derive(FromQueryResult)]
pub(super) struct BandRow {
    pub(super) parameter_id: Uuid,
    pub(super) deployment_id: Uuid,
    pub(super) sensor_id: Uuid,
    pub(super) sensor_serial: Option<String>,
    pub(super) sensor_name: Option<String>,
    pub(super) site_id: Uuid,
    pub(super) deployed_from: DateTime<chrono::FixedOffset>,
    pub(super) deployed_until: Option<DateTime<chrono::FixedOffset>>,
}

#[derive(FromQueryResult)]
pub(super) struct MarkerRow {
    pub(super) parameter_id: Uuid,
    pub(super) calibration_id: Uuid,
    pub(super) sensor_id: Uuid,
    pub(super) slope: f64,
    pub(super) intercept: f64,
    pub(super) valid_from: DateTime<chrono::FixedOffset>,
    pub(super) valid_until: Option<DateTime<chrono::FixedOffset>>,
}

#[cfg(test)]
#[path = "tests/models.rs"]
mod tests;
