//! The instrument entity and every shape the component puts on the wire.

use chrono::{DateTime, Utc};
use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::service::SensorOperations;

#[derive(Clone, Debug, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels)]
#[sea_orm(table_name = "sensors")]
#[crudcrate(
    api_struct = "Sensor",
    name_singular = "sensor",
    name_plural = "sensors",
    generate_router,
    operations = SensorOperations
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable, fulltext, sortable)]
    pub serial_number: Option<String>,
    #[crudcrate(fulltext, sortable)]
    pub name: Option<String>,
    #[crudcrate(filterable)]
    pub manufacturer: Option<String>,
    #[crudcrate(filterable)]
    pub model: Option<String>,
    #[crudcrate(filterable)]
    pub is_active: Option<bool>,
    #[crudcrate(filterable)]
    pub is_lab_instrument: Option<bool>,
    /// What this row is: `device` (a physical instrument), `lab` (a portal curve label),
    /// `source_parameter` (a source's instrument for one parameter across every station) or
    /// `entry_channel` (a slot's hand-entry channel). Written by whichever path minted it;
    /// `(source_system, source_key)` stays the identity.
    #[crudcrate(filterable, sortable, on_create = "device".to_string())]
    pub kind: String,
    /// Sync provenance: the source a replicated lab instrument came from (e.g. "cnet"). NULL on
    /// devices registered by serial. Written only by the sync paths, never through CRUD.
    #[crudcrate(exclude(create, update), filterable)]
    pub source_system: Option<String>,
    /// The instrument's identity within its source (e.g. "cnet:DOC corr"); the lookup key
    /// together with `source_system`.
    #[crudcrate(exclude(create, update), filterable)]
    pub source_key: Option<String>,
    /// Cadence classification: 'high' (field stream → continuous readings) or 'low'
    /// (lab/campaign → spot readings). Resolved at ingest for streams owned by this sensor.
    #[crudcrate(filterable, sortable, on_create = "high".to_string())]
    pub data_frequency: String,
    pub notes: Option<String>,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub metadata: Option<serde_json::Value>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update), join(one, depth = 1))]
    pub deployments: Vec<crate::routes::private::sensors::deployments::SensorDeployment>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub reading_count: Option<i64>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub last_reading_at: Option<chrono::DateTime<chrono::Utc>>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub last_calibration_at: Option<chrono::DateTime<chrono::Utc>>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub current_site_id: Option<Uuid>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub current_site_name: Option<String>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub last_reading_value: Option<f64>,
    /// Standard curves fitted on this instrument. A lab instrument's row states this where a
    /// device's states its site: a titrator has no deployment and the Site column is empty for it
    /// however long it has been in use.
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub curve_count: Option<i64>,
    /// The newest reading any of those curves corrected: whether the instrument is still in use.
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub last_curve_use: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "crate::routes::private::sensors::calibrations::Entity")]
    SensorCalibrations,
    #[sea_orm(has_many = "crate::routes::private::sensors::deployments::Entity")]
    SensorDeployments,
    #[sea_orm(has_many = "crate::routes::private::readings::Entity")]
    Readings,
}

impl Related<crate::routes::private::sensors::calibrations::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SensorCalibrations.def()
    }
}

impl Related<crate::routes::private::sensors::deployments::Entity> for Entity {
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

/// Resolved sensor context for readings.
#[derive(Debug, Clone)]
pub struct SensorContext {
    pub sensor_id: Uuid,
    /// `None` when the sensor is not deployed to the target site at this time, the slot may be
    /// occupied by another sensor, or the sensor isn't adopted yet. Readings still carry
    /// `sensor_id`; the deployment FK is absent.
    pub deployment_id: Option<Uuid>,
}

/// What an instrument row is. Three of the four minting paths produce something that is not a
/// device, and a picker offering all four asks an operator to tell a spectrophotometer from a
/// bookkeeping row with nothing to go on. It is a display and selection attribute:
/// `(source_system, source_key)` stays the identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstrumentKind {
    /// A physical instrument: a probe, a logger channel, a lab device with a serial.
    Device,
    /// A portal curve label, one row per analyte.
    Lab,
    /// A source's instrument for one parameter, across every station it reports.
    SourceParameter,
    /// A slot's own hand-entry channel (grab entry, API batch).
    EntryChannel,
}

impl InstrumentKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Device => "device",
            Self::Lab => "lab",
            Self::SourceParameter => "source_parameter",
            Self::EntryChannel => "entry_channel",
        }
    }

    /// Everything that is not a field device has sat under this flag since before the kinds were
    /// distinguished. Still written on every mint so a reader of the column alone sees what it
    /// always saw; nothing decides anything from it.
    #[must_use]
    pub fn is_lab_instrument(self) -> bool {
        self != Self::Device
    }

    /// The kind a stored row is, from the column that holds the fact and, for a row predating the
    /// backfill that filled it, from the flag that is its shadow. The flag cannot tell `lab` from
    /// the two bookkeeping kinds, so an unrecognised kind resolves to `Lab` exactly where the flag
    /// is set and `Device` otherwise: the same fallback the client draws.
    #[must_use]
    pub fn of(kind: Option<&str>, is_lab_instrument: Option<bool>) -> Self {
        match kind {
            Some("device") => Self::Device,
            Some("lab") => Self::Lab,
            Some("source_parameter") => Self::SourceParameter,
            Some("entry_channel") => Self::EntryChannel,
            _ if is_lab_instrument.unwrap_or(false) => Self::Lab,
            _ => Self::Device,
        }
    }

    /// Whether a row stands for something an operator could have measured on. The two bookkeeping
    /// kinds exist so a reading can name an instrument at all, and nothing was measured on them.
    #[must_use]
    pub fn is_bookkeeping(self) -> bool {
        matches!(self, Self::SourceParameter | Self::EntryChannel)
    }
}

/// One resolved attribution slot for a reading time.
#[derive(Debug, Clone, Default)]
pub struct ResolvedSlot {
    pub calibration_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
    pub site_id: Option<Uuid>,
}

/// Owner (sensor + deployment + active calibration) resolved for a reading time at a slot.
#[derive(Debug, Clone, Default)]
pub struct ResolvedOwner {
    pub sensor_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
    pub calibration_id: Option<Uuid>,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Adopt
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct AdoptRequest {
    pub site_id: Uuid,
    /// The parameter this deployment binds the sensor to. Optional: derived from the sensor's
    /// existing single parameter when omitted; required when the sensor covers several.
    #[serde(default)]
    pub parameter_id: Option<Uuid>,
    /// Half-open window start. Defaults to now().
    #[serde(default)]
    pub deployed_from: Option<DateTime<Utc>>,
    /// Optional window end (recall). NULL = open-ended.
    #[serde(default)]
    pub deployed_until: Option<DateTime<Utc>>,
    /// Auto-create the (site, parameter) site_parameter if missing. Default true.
    #[serde(default = "default_true")]
    pub create_site_parameter: bool,
    #[serde(default)]
    pub deployment_type: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AdoptResponse {
    pub deployment_id: Uuid,
    pub sensor_id: Uuid,
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub site_parameter_id: Uuid,
    pub site_parameter_created: bool,
    pub deployed_from: DateTime<Utc>,
    #[schema(required)]
    pub deployed_until: Option<DateTime<Utc>>,
    pub job_id: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AdoptSuggestion {
    pub now: DateTime<Utc>,
    #[schema(required)]
    pub end_of_last_deployment: Option<DateTime<Utc>>,
    #[schema(required)]
    pub first_reading: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Swap (end A, start B)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct SwapRequest {
    pub outgoing_sensor_id: Uuid,
    pub incoming_sensor_id: Uuid,
    pub site_id: Uuid,
    /// The (site, parameter) slot to swap. Optional: derived from the outgoing sensor's deployment at
    /// the site when omitted.
    #[serde(default)]
    pub parameter_id: Option<Uuid>,
    /// Instant of the swap. Defaults to now(). A ends at T, B starts at T (half-open => no overlap).
    #[serde(default)]
    pub at: Option<DateTime<Utc>>,
    #[serde(default = "default_true")]
    pub create_site_parameter: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SwapResponse {
    #[schema(required)]
    pub ended_deployment_id: Option<Uuid>,
    pub started_deployment_id: Uuid,
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub at: DateTime<Utc>,
    #[schema(required)]
    pub outgoing_job_id: Option<Uuid>,
    pub incoming_job_id: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CurveOverview {
    pub id: Uuid,
    #[schema(required)]
    pub name: Option<String>,
    pub slope: f64,
    pub intercept: f64,
    #[schema(required)]
    pub r_squared: Option<f64>,
    #[schema(required)]
    pub source_system: Option<String>,
    #[schema(required)]
    pub source_key: Option<String>,
    #[schema(required)]
    pub created_at: Option<DateTime<Utc>>,
    /// Readings this curve corrected.
    pub reading_count: i64,
    #[schema(required)]
    pub first_used: Option<DateTime<Utc>>,
    #[schema(required)]
    pub last_used: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InstrumentStreamRef {
    pub id: Uuid,
    pub source_system: String,
    pub source_key: String,
    #[schema(required)]
    pub measurement_type: Option<String>,
    /// The paired slot, when the stream has one.
    #[schema(required)]
    pub site_name: Option<String>,
    #[schema(required)]
    pub parameter_code: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InstrumentOverview {
    pub id: Uuid,
    #[schema(required)]
    pub name: Option<String>,
    #[schema(required)]
    pub serial_number: Option<String>,
    #[schema(required)]
    pub manufacturer: Option<String>,
    #[schema(required)]
    pub model: Option<String>,
    /// Kept as the flag has always read: everything that is not a field device. `kind` is what
    /// says which of the three.
    pub is_lab_instrument: bool,
    /// What this row is: `device`, `lab`, `source_parameter` or `entry_channel`. Resolved from the
    /// stored column, falling back to the flag for a row predating the backfill.
    pub kind: String,
    #[schema(required)]
    pub source_system: Option<String>,
    #[schema(required)]
    pub source_key: Option<String>,
    pub curves: Vec<CurveOverview>,
    pub streams: Vec<InstrumentStreamRef>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InstrumentsOverviewResponse {
    pub instruments: Vec<InstrumentOverview>,
}

/// One reading a standard curve corrected.
#[derive(Debug, Serialize, ToSchema)]
pub struct CurveUsagePoint {
    pub time: DateTime<Utc>,
    pub replicate_index: i16,
    pub raw_value: f64,
    #[schema(required)]
    pub calibrated_value: Option<f64>,
    pub is_flagged: bool,
    #[schema(required)]
    pub site_name: Option<String>,
    #[schema(required)]
    pub parameter_code: Option<String>,
}

/// The readings a standard curve corrected. `points` is capped (most recent first) while
/// `reading_count` is the true total.
#[derive(Debug, Serialize, ToSchema)]
pub struct CurveUsageResponse {
    pub curve_id: Uuid,
    pub sensor_id: Uuid,
    pub slope: f64,
    pub intercept: f64,
    pub reading_count: i64,
    pub points: Vec<CurveUsagePoint>,
}

/// One curve's usage on the instrument that owns it.
#[derive(Debug, Serialize, ToSchema)]
pub struct SensorCurveUsage {
    pub curve_id: Uuid,
    /// Readings this curve corrected.
    pub reading_count: i64,
    #[schema(required)]
    pub first_used: Option<DateTime<Utc>>,
    #[schema(required)]
    pub last_used: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SensorCurveUsageResponse {
    pub sensor_id: Uuid,
    pub usage: Vec<SensorCurveUsage>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct SensorReadingsQuery {
    /// Start time (ISO 8601). Defaults to the earliest reading.
    pub start: Option<DateTime<Utc>>,
    /// End time (ISO 8601). Defaults to now.
    pub end: Option<DateTime<Utc>>,
    /// Include the per-point raw (uncalibrated) value array. Default true.
    pub include_raw: Option<bool>,
    /// Downsampling resolution: `raw` (default, per-point), or `hourly`/`daily`/`weekly`/`monthly`
    /// time-bucketed averages with min/max envelopes. Mirrors the site plot's resolution selector.
    pub resolution: Option<String>,
    /// Filter by measurement type: continuous, spot, derived. Omit for all types at `raw`
    /// resolution; bucketed resolutions always exclude spot (continuous-aggregate semantics).
    pub measurement_type: Option<String>,
    /// Which channel of a multi-parameter instrument to serve. Defaults to the parameter of the
    /// sensor's most recent deployment, which is the one the response names.
    pub parameter_id: Option<Uuid>,
}

/// Columnar raw + calibrated series for one channel of a sensor, aligned to `times`.
/// `site_ids[i]` is the site the reading was attributed to (null when the sensor was undeployed at
/// that time). In an aggregated `resolution`, `raw`/`calibrated` are the per-bucket averages and
/// the `*_min`/`*_max` envelopes are populated (empty in `raw` mode).
#[derive(Debug, Serialize, ToSchema)]
pub struct SensorReadingsResponse {
    pub sensor_id: Uuid,
    #[schema(required)]
    pub parameter_id: Option<Uuid>,
    #[schema(required)]
    pub units: Option<String>,
    /// Resolution actually applied (`raw`, `hourly`, `daily`, `weekly`, `monthly`).
    pub resolution: String,
    pub times: Vec<DateTime<Utc>>,
    pub raw: Vec<Option<f64>>,
    pub calibrated: Vec<Option<f64>>,
    pub raw_min: Vec<Option<f64>>,
    pub raw_max: Vec<Option<f64>>,
    pub calibrated_min: Vec<Option<f64>>,
    pub calibrated_max: Vec<Option<f64>>,
    pub site_ids: Vec<Option<Uuid>>,
    /// Earliest/latest reading time **attributed to this sensor** on the served parameter (full
    /// extent, independent of the query window).
    #[schema(required)]
    pub data_start: Option<DateTime<Utc>>,
    #[schema(required)]
    pub data_end: Option<DateTime<Utc>>,
    /// Earliest reading at the sensor's current (open) deployment slot, same site + parameter,
    /// regardless of `sensor_id`. This is the true backdate target: history before `data_start`
    /// that is not yet attributed to the sensor but would be claimed by backdating `deployed_from`.
    /// Null when the sensor has no open deployment.
    #[schema(required)]
    pub slot_data_start: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct SensorBandsQuery {
    /// Clip bands to this start (ISO 8601).
    pub start: Option<DateTime<Utc>>,
    /// Clip bands to this end (ISO 8601).
    pub end: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SensorDeploymentBand {
    pub deployment_id: Uuid,
    pub site_id: Uuid,
    #[schema(required)]
    pub site_name: Option<String>,
    pub from: DateTime<Utc>,
    #[schema(required)]
    pub until: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SensorDeploymentBandsResponse {
    pub sensor_id: Uuid,
    pub bands: Vec<SensorDeploymentBand>,
}

/// One instrument from a source's own register. The instrument's own fields are
/// `river_data_core::models::SensorUpsert`, which the sync services build from; the API adds the
/// source the caller is speaking for, and supplies the `is_lab_instrument` default this route has
/// always accepted an omitted flag under.
#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterSensorRequest {
    /// The sync source the instrument comes from, e.g. "metalp".
    pub source_system: String,
    #[serde(flatten)]
    pub instrument: river_data_core::models::SensorUpsert,
}

impl<'de> Deserialize<'de> for RegisterSensorRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (source_system, instrument) = crate::routes::private::wire::with_source_system(
            deserializer,
            &[("is_lab_instrument", serde_json::json!(false))],
        )?;
        Ok(Self {
            source_system,
            instrument,
        })
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterSensorResponse {
    pub id: Uuid,
    /// False when the provenance key already named an instrument, in which case nothing on it was
    /// changed.
    pub created: bool,
    /// The instrument already holding the serial this registration offered, when that is why the
    /// serial was not claimed. The source's register is wrong or the two rows are one instrument;
    /// either way it is a person's call, so the registration succeeds and says so.
    #[schema(required)]
    pub serial_claimed_by: Option<Uuid>,
}

/// A source's whole instrument register, offered for a plan to admit.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ProposeInstrumentsRequest {
    /// The sync source the register belongs to, e.g. "metalp".
    pub source_system: String,
    pub instruments: Vec<river_data_core::models::SensorUpsert>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProposeInstrumentsResponse {
    /// Rows now held as proposals, whether this call created or refreshed them.
    pub stored: usize,
    /// Proposals a plan has already admitted, which are instruments now and are left alone.
    pub already_admitted: usize,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RetagFrequencyRequest {
    pub sensor_ids: Vec<Uuid>,
    /// 'high' (field stream → continuous) or 'low' (lab/campaign → spot).
    pub data_frequency: String,
    /// Also retag the sensors' existing readings and refresh aggregates (tracked job).
    /// When false only future ingestion is affected.
    #[serde(default)]
    pub retag_existing: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RetagFrequencyResponse {
    pub sensors_updated: u64,
    pub data_frequency: String,
    /// The tracked `measurement_retag` job, when `retag_existing` was requested.
    #[schema(required)]
    pub job_id: Option<Uuid>,
}

/// An instrument a source has offered and no pairing plan has admitted yet (Q134).
pub mod proposal {
    use crudcrate::EntityToModels;
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
    #[sea_orm(table_name = "instrument_proposals")]
    #[crudcrate(
        api_struct = "InstrumentProposal",
        name_singular = "instrument_proposal",
        name_plural = "instrument_proposals"
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable)]
        pub source_system: String,
        #[crudcrate(filterable)]
        pub source_key: String,
        pub name: String,
        pub serial_number: Option<String>,
        pub manufacturer: Option<String>,
        pub model: Option<String>,
        pub notes: Option<String>,
        pub is_lab_instrument: bool,
        pub data_frequency: Option<String>,
        pub metadata: Option<serde_json::Value>,
        #[crudcrate(exclude(create, update), sortable)]
        pub first_seen_at: chrono::DateTime<chrono::Utc>,
        #[crudcrate(exclude(create, update), sortable)]
        pub last_seen_at: chrono::DateTime<chrono::Utc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

#[cfg(test)]
#[path = "tests/models.rs"]
mod tests;
