use std::collections::{BTreeMap, HashMap, HashSet};

use axum::{
    Json,
    extract::{Query, State},
};
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, Statement};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::error::{AppError, AppResult};
use crate::routes::private::readings::samples;
use crate::routes::private::sensors::{calibrations, deployments, standard_curves};
use crate::routes::private::{collection_events, data_streams, sensors, sites};

/// One instant of one series, addressed either by the readings PK's stream half or by the slot a
/// chart knows. The whole replicate group at the instant is the record.
#[derive(Debug, Deserialize, IntoParams)]
pub struct ProvenanceQuery {
    /// The instant (exact reading timestamp).
    pub time: DateTime<Utc>,
    /// Key form 1: the stream serving the point.
    pub stream_id: Option<Uuid>,
    /// Key form 2: the site half of the slot (with `parameter_id`).
    pub site_id: Option<Uuid>,
    /// Key form 2: the parameter half of the slot (with `site_id`).
    pub parameter_id: Option<Uuid>,
    /// Narrow key form 2 to one cadence ('continuous' matches rows stored as NULL).
    pub measurement_type: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProvenanceResponse {
    pub time: DateTime<Utc>,
    #[schema(required)]
    pub site_id: Option<Uuid>,
    #[schema(required)]
    pub parameter_id: Option<Uuid>,
    /// What the instant measured, named. The record was serving bare numbers under a bare uuid.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub parameter_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub parameter_name: Option<String>,
    /// The slot's unit when it declares one, the catalog default otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub units: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub decimal_places: Option<i16>,
    /// More than one stream serves this (site, parameter) at this instant.
    pub duplicate_slot: bool,
    /// One record per stream serving the instant.
    pub records: Vec<ProvenanceRecord>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProvenanceRecord {
    pub origin: OriginInfo,
    pub readings: Vec<ReadingFacet>,
    pub chain: ChainInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub event: Option<EventRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub computation: Option<ComputationInfo>,
    /// The formula that produced a derived value, the counterpart of a tool run's record.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub calculation: Option<CalculationInfo>,
    /// The values the formula producing this parameter read at this instant, one hop up the
    /// chain. Each names the key its own record is read by.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub inputs: Vec<InputRef>,
    /// Every enabled formula reading this parameter, one hop down the chain, with its output's
    /// value at this instant where one exists.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub consumers: Vec<ConsumerRef>,
    pub holds: Vec<HoldRef>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OriginInfo {
    pub stream_id: Uuid,
    pub source_system: String,
    pub source_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub source_name: Option<String>,
    /// 'sync' | 'manual' | 'csv' | 'api', from the stream's source system.
    pub classification: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub paired_at: Option<DateTime<Utc>>,
    /// Latest first-arrival stamp in the replicate group: when these rows first existed. NULL
    /// means they predate tracking. Nothing moves it, so a corrected row still reports its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub ingested_at: Option<DateTime<Utc>>,
    /// When the value the group currently serves arrived: the latest live value correction's `at`
    /// where one exists, and the first arrival otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub value_arrived_at: Option<DateTime<Utc>>,
    /// The latest windowed-ingest pass whose claimed window covers the instant.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub receipt: Option<ReceiptSummary>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReceiptSummary {
    pub id: Uuid,
    pub at: DateTime<Utc>,
    #[schema(required)]
    pub window_from: Option<DateTime<Utc>>,
    #[schema(required)]
    pub window_to: Option<DateTime<Utc>>,
    pub submitted: i32,
    pub new_rows: i32,
    pub changed: i32,
    pub unchanged: i32,
    pub withdrawn: i32,
    pub rejected_total: i32,
    pub braked: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReadingFacet {
    pub replicate_index: i16,
    pub raw_value: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub calibrated_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub measurement_type: Option<String>,
    pub is_flagged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub flag_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub withdrawn_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub withdrawn_reason: Option<String>,
    /// A pending entry: stored and shown here, never published (Q18, Q21).
    pub unverified: bool,
    /// When this row first existed. Nothing moves it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub ingested_at: Option<DateTime<Utc>>,
    /// When the value this row currently serves arrived: its latest live value correction's `at`,
    /// else its first arrival.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub value_arrived_at: Option<DateTime<Utc>>,
    /// Where this value came from, one of `PROVENANCE_KINDS`. A `sync` or `derived` row's story is
    /// resolved from the stream, the receipt and the definition; the others carry a stored blob.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub provenance_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub calibration: Option<CalibrationRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub standard_curve: Option<CurveRef>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CalibrationRef {
    pub id: Uuid,
    pub slope: f64,
    pub intercept: f64,
    pub valid_from: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub valid_until: Option<DateTime<Utc>>,
    /// Set when the curve has been retired: the reading keeps the value it produced, and no new
    /// measurement resolves it (M146).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub retired_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CurveRef {
    pub id: Uuid,
    /// The lab instrument the curve belongs to, which is where its record lives.
    pub sensor_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub name: Option<String>,
    pub slope: f64,
    pub intercept: f64,
    /// Set when the lab has taken the curve out of circulation (M147). The value stands.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub retired_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChainInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub sensor: Option<SensorRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub deployment: Option<DeploymentRef>,
    /// Live instrument or calibration pins on the group: attribution a person decided, which
    /// reprocess leaves alone.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub pins: Vec<PinRef>,
}

/// When the value at each replicate last changed: the latest live `value_correction`'s `at`,
/// keyed by `(stream_id, replicate_index)`. `ingested_at` is the row's first arrival and no
/// decision moves it, so this is what says when the number now served appeared.
async fn load_value_arrivals(
    db: &sea_orm::DatabaseConnection,
    stream_ids: &[Uuid],
    at: DateTime<Utc>,
) -> AppResult<HashMap<(Uuid, i16), DateTime<Utc>>> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT stream_id, replicate_index, MAX(at) AS at
             FROM reading_decisions
             WHERE stream_id = ANY($1) AND time = $2
               AND kind = 'value_correction' AND rolled_back_by IS NULL
               AND replicate_index IS NOT NULL
             GROUP BY stream_id, replicate_index",
            [
                stream_ids.to_vec().into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(at).into(),
            ],
        ))
        .await?;
    let mut out = HashMap::new();
    for r in rows.iter().map(|r| ArrivalRow::from_query_result(r, "")) {
        let r = r?;
        out.insert((r.stream_id, r.replicate_index), r.at.with_timezone(&Utc));
    }
    Ok(out)
}

#[derive(FromQueryResult)]
struct ArrivalRow {
    stream_id: Uuid,
    replicate_index: i16,
    at: DateTime<chrono::FixedOffset>,
}

/// Live instrument and calibration pins on the streams' instant (ADR 0008, M59), keyed by stream.
async fn load_pins(
    db: &sea_orm::DatabaseConnection,
    stream_ids: &[Uuid],
    at: DateTime<Utc>,
) -> AppResult<HashMap<Uuid, Vec<PinRef>>> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT stream_id, id, kind, replicate_index, new, actor, at, reason, set_id
             FROM reading_decisions
             WHERE stream_id = ANY($1) AND time = $2
               AND kind IN ('instrument_pin', 'calibration_pin') AND rolled_back_by IS NULL
             ORDER BY at DESC",
            [
                stream_ids.to_vec().into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(at).into(),
            ],
        ))
        .await?;
    let mut out: HashMap<Uuid, Vec<PinRef>> = HashMap::new();
    for r in rows.iter().map(|r| PinRow::from_query_result(r, "")) {
        let r = r?;
        out.entry(r.stream_id).or_default().push(PinRef {
            decision_id: r.id,
            kind: r.kind,
            replicate_index: r.replicate_index,
            target: r.new,
            actor: r.actor,
            at: r.at.with_timezone(&Utc),
            reason: r.reason,
            set_id: r.set_id,
        });
    }
    Ok(out)
}

#[derive(FromQueryResult)]
struct PinRow {
    stream_id: Uuid,
    id: Uuid,
    kind: String,
    replicate_index: Option<i16>,
    new: serde_json::Value,
    actor: String,
    at: DateTime<chrono::FixedOffset>,
    reason: Option<String>,
    set_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PinRef {
    pub decision_id: Uuid,
    /// `instrument_pin` | `calibration_pin`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub replicate_index: Option<i16>,
    #[schema(value_type = Object)]
    pub target: serde_json::Value,
    pub actor: String,
    pub at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub set_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SensorRef {
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub serial_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub manufacturer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub model: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeploymentRef {
    pub id: Uuid,
    pub site_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub site_name: Option<String>,
    pub deployed_from: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub deployed_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EventRef {
    pub id: Uuid,
    pub collected_at: DateTime<Utc>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub created_by: Option<String>,
}

/// The standalone formula behind a derived value: the definition it belongs to and the version it
/// was made with. A row stored before versioning names no version, so `formula` is absent rather
/// than filled from the definition's current text, which no longer describes it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CalculationInfo {
    pub definition_id: Uuid,
    pub code: String,
    pub name: String,
    /// The version the stored value names, absent when the value predates versioning.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub version_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub version_no: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub formula: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub content_hash: Option<String>,
    /// The definition's newest version, so a value made by an older one is visible as such.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub active_version_no: Option<i32>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ComputationInfo {
    /// The statistics row, when the instant carries two or more replicates.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub sample_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub created_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub notes: Option<String>,
    /// The server-built tool-run blob stored on the reading, verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, value_type = Option<HashMap<String, serde_json::Value>>)]
    pub provenance: Option<serde_json::Value>,
    /// The run's minting path: 'interactive' | 'csv_import' | 'chain'.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub run_source: Option<String>,
    /// Which divisor this group's served standard deviation uses ('sample' = n-1, 'population' =
    /// n) and what chose it. `sd_estimator_source` 'default' means nothing declared one, so the
    /// number is served under a convention nobody stated. Absent on a single measurement, which
    /// has no standard deviation.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub sd_estimator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub sd_estimator_source: Option<String>,
    /// The group's statistics, the numbers the chart plotted and drew its bar from. Without these
    /// the record shows the replicates and a sentence about the divisor, and never what was served.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub n: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub mean: Option<f64>,
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
}

#[derive(Debug, Serialize, ToSchema)]
pub struct HoldRef {
    pub id: Uuid,
    pub kind: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

/// One value a formula read at the record's instant. A parameter input carries the slot key
/// (`parameter_id` with the record's site and time) that resolves its own record; a site property
/// carries the column it was read from.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct InputRef {
    pub definition_id: Uuid,
    /// The formula's code, so two formulas producing one parameter stay apart.
    pub formula_code: String,
    pub variable_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub parameter_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub parameter_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub site_property: Option<String>,
    /// Set when the formula evaluates per replicate and this is the variable it iterates: the
    /// value at this index fed the output at the same index.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub replicate_index: Option<i16>,
    /// How the value was read: `replicate` (the row at the index), `mean` (the family's sample
    /// statistic), `reading` (the single row at the instant), `site` (the site row's column).
    /// `missing` when nothing at the instant answers.
    pub served_as: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub value: Option<f64>,
}

/// One formula reading the record's parameter, with its output at the instant where one exists.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConsumerRef {
    pub definition_id: Uuid,
    pub formula_code: String,
    pub formula_name: String,
    /// The calculation the formula belongs to, absent on a standalone derived parameter.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub calculation: Option<String>,
    pub variable_name: String,
    /// The slot key of the output's own record. Absent on an intermediate, which mints none.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub output_parameter_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub output_parameter_code: Option<String>,
    /// Set when the formula evaluates per replicate over this parameter: the output at this index
    /// came from the record's value at the same index.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub replicate_index: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub value: Option<f64>,
}

/// The stream's origin class, from the writer-side source-system set in
/// `collection_events::attach`.
/// How a reading reached the store, from the stream it arrived on: `manual`, `csv`, `api`,
/// `derived` or `sync`. One definition, so every surface naming an origin names the same thing. A
/// computed value was made here rather than sent, so it is not a sync.
pub fn classify_source(source_system: &str) -> &'static str {
    match source_system {
        "grab_sample" => "manual",
        "csv" | "csv_import" => "csv",
        "api" => "api",
        "derived" => "derived",
        _ => "sync",
    }
}

/// Where a reading came from, as the row itself records it.
///
/// Q49: a blob is stored only where nothing else records the story (a tool run, a chain, a CSV
/// import, a hand entry, a batch); a sync or derived reading's story is resolved from the stream,
/// the covering receipt and the definition. The discriminator is stored on every row either way,
/// so an origin nothing recorded is a named kind rather than a NULL blob.
pub const PROVENANCE_KINDS: [&str; 8] = [
    "tool_run",
    "chain",
    "csv_import",
    "manual",
    "batch",
    "sync",
    "derived",
    "migration",
];

/// The kind a writer with no better evidence stamps, from the row's own classification and the
/// stream it arrived on. Mirrors `readings_default_provenance_kind`, the trigger that holds the
/// column total for a writer that names none.
#[must_use]
pub fn provenance_kind_for_stream(
    measurement_type: Option<&str>,
    source_system: Option<&str>,
) -> &'static str {
    if measurement_type == Some("derived") {
        return "derived";
    }
    match source_system {
        Some("grab_sample") => "manual",
        Some("api") => "batch",
        Some(_) => "sync",
        None => "migration",
    }
}

/// The kind of a save that names a tool run, from the run's own minting path
/// (`tool_runs.source`). A hand entry that names no run is `manual`.
#[must_use]
pub fn provenance_kind_for_run(run_source: Option<&str>) -> &'static str {
    match run_source {
        None => "manual",
        Some("chain") => "chain",
        Some("csv_import") => "csv_import",
        Some(_) => "tool_run",
    }
}

#[derive(Debug, FromQueryResult)]
pub struct RawRow {
    pub stream_id: Uuid,
    replicate_index: i16,
    pub site_id: Option<Uuid>,
    pub parameter_id: Option<Uuid>,
    raw_value: f64,
    calibrated_value: Option<f64>,
    sensor_id: Option<Uuid>,
    calibration_id: Option<Uuid>,
    standard_curve_id: Option<Uuid>,
    deployment_id: Option<Uuid>,
    measurement_type: Option<String>,
    is_flagged: Option<bool>,
    flag_reason: Option<String>,
    pub sample_id: Option<Uuid>,
    pub collection_event_id: Option<Uuid>,
    withdrawn_at: Option<DateTime<Utc>>,
    unverified: Option<bool>,
    withdrawn_reason: Option<String>,
    ingested_at: Option<DateTime<Utc>>,
    provenance_kind: Option<String>,
    pub provenance: Option<serde_json::Value>,
    derived_version_id: Option<Uuid>,
    label: Option<String>,
    notes: Option<String>,
    created_by: Option<String>,
}

const ROW_COLUMNS: &str = "stream_id, replicate_index, site_id, parameter_id, raw_value, \
     calibrated_value, sensor_id, calibration_id, standard_curve_id, deployment_id, \
     measurement_type, is_flagged, flag_reason, sample_id, collection_event_id, \
     withdrawn_at, withdrawn_reason, unverified, ingested_at, provenance_kind, provenance, \
     derived_version_id, label, notes, created_by";

/// The assembled record of one measured instant: where it came from, when it arrived, what
/// instrument and corrections produced the stored value, the visit it belongs to, the tool run
/// that computed it, and any review holds touching it. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/api/readings/provenance",
    params(ProvenanceQuery),
    responses(
        (status = 200, description = "Provenance record", body = ProvenanceResponse),
        (status = 400, description = "Neither key form provided"),
        (status = 404, description = "No reading at that instant"),
    ),
    tag = "readings"
)]
pub async fn get_reading_provenance(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(q): Query<ProvenanceQuery>,
) -> AppResult<Json<ProvenanceResponse>> {
    let rows = rows_at(&state.db, &q, &scope).await?;
    let records = assemble_records(&state.db, &rows, q.time).await?;

    let site_id = rows.iter().find_map(|r| r.site_id).or(q.site_id);
    let parameter_id = rows.iter().find_map(|r| r.parameter_id).or(q.parameter_id);
    let slot = match (site_id, parameter_id) {
        (Some(site_id), Some(parameter_id)) => {
            slot_identity(&state.db, site_id, parameter_id).await?
        }
        _ => None,
    };

    Ok(Json(ProvenanceResponse {
        time: q.time,
        site_id,
        parameter_id,
        parameter_code: slot.as_ref().map(|s| s.0.clone()),
        parameter_name: slot.as_ref().map(|s| s.1.clone()),
        units: slot.as_ref().and_then(|s| s.2.clone()),
        decimal_places: slot.as_ref().and_then(|s| s.3),
        duplicate_slot: records.len() > 1,
        records,
    }))
}

/// The replicate group at the instant, by either key form, refused to a scoped caller who may not
/// see it. Both the record and the ledger start from these rows, so they can never disagree about
/// which reading was asked for.
pub async fn rows_at(
    db: &sea_orm::DatabaseConnection,
    q: &ProvenanceQuery,
    scope: &crate::common::authz::AccessScope,
) -> AppResult<Vec<RawRow>> {
    let rows: Vec<RawRow> = match (q.stream_id, q.site_id, q.parameter_id) {
        (Some(stream_id), _, _) => {
            let stmt = Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT {ROW_COLUMNS} FROM readings WHERE stream_id = $1 AND time = $2 \
                     ORDER BY replicate_index"
                ),
                [stream_id.into(), q.time.into()],
            );
            db.query_all_raw(stmt).await?
        }
        (None, Some(site_id), Some(parameter_id)) => {
            let cadence = match q.measurement_type.as_deref() {
                None => String::new(),
                // The same word the readings query serves under: everything that is not a grab.
                // A derived row plots on the continuous line, so a chart that drew it must be able
                // to resolve the point it drew.
                Some("continuous") => " AND (measurement_type IS DISTINCT FROM 'spot')".into(),
                Some(other) => format!(" AND measurement_type = '{}'", sanitize_cadence(other)?),
            };
            let stmt = Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT {ROW_COLUMNS} FROM readings \
                     WHERE site_id = $1 AND parameter_id = $2 AND time = $3{cadence} \
                     ORDER BY stream_id, replicate_index"
                ),
                [site_id.into(), parameter_id.into(), q.time.into()],
            );
            db.query_all_raw(stmt).await?
        }
        _ => {
            return Err(AppError::BadRequest(
                "Provide either stream_id or both site_id and parameter_id".to_string(),
            ));
        }
    }
    .iter()
    .map(|row| RawRow::from_query_result(row, ""))
    .collect::<Result<_, _>>()?;

    if rows.is_empty() {
        return Err(AppError::NotFound("No reading at that instant".to_string()));
    }

    // A project-scoped key sees another project's data (or unattributed rows) as not-found.
    if scope.is_restricted() {
        let project = match rows.iter().find_map(|r| r.site_id) {
            Some(site_id) => sites::Entity::find_by_id(site_id)
                .one(db)
                .await?
                .and_then(|s| s.project_id),
            None => None,
        };
        if !scope.allows_project_opt(project) {
            return Err(AppError::NotFound("No reading at that instant".to_string()));
        }
    }
    Ok(rows)
}

/// The readings of one instant, grouped by stream into assembled records. Every lookup is batched
/// over the whole row set, so a visit's twenty cells cost the same number of queries as one.
pub async fn assemble_records(
    db: &sea_orm::DatabaseConnection,
    rows: &[RawRow],
    time: DateTime<Utc>,
) -> AppResult<Vec<ProvenanceRecord>> {
    // --- Batch-resolve everything the rows reference ---
    let mut groups: BTreeMap<Uuid, Vec<&RawRow>> = BTreeMap::new();
    for r in rows {
        groups.entry(r.stream_id).or_default().push(r);
    }
    let collect = |f: fn(&RawRow) -> Option<Uuid>| -> Vec<Uuid> {
        rows.iter()
            .filter_map(f)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    };

    let streams: HashMap<Uuid, data_streams::Model> = data_streams::Entity::find()
        .filter(data_streams::Column::Id.is_in(groups.keys().copied().collect::<Vec<_>>()))
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.id, s))
        .collect();
    let sensor_map: HashMap<Uuid, sensors::Model> = sensors::Entity::find()
        .filter(sensors::Column::Id.is_in(collect(|r| r.sensor_id)))
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.id, s))
        .collect();
    let deployment_map: HashMap<Uuid, deployments::Model> = deployments::Entity::find()
        .filter(deployments::Column::Id.is_in(collect(|r| r.deployment_id)))
        .all(db)
        .await?
        .into_iter()
        .map(|d| (d.id, d))
        .collect();
    let calibration_map: HashMap<Uuid, calibrations::Model> = calibrations::Entity::find()
        .filter(calibrations::Column::Id.is_in(collect(|r| r.calibration_id)))
        .all(db)
        .await?
        .into_iter()
        .map(|c| (c.id, c))
        .collect();
    let curve_map: HashMap<Uuid, standard_curves::Model> = standard_curves::Entity::find()
        .filter(standard_curves::Column::Id.is_in(collect(|r| r.standard_curve_id)))
        .all(db)
        .await?
        .into_iter()
        .map(|c| (c.id, c))
        .collect();
    let event_map: HashMap<Uuid, collection_events::Model> = collection_events::Entity::find()
        .filter(collection_events::Column::Id.is_in(collect(|r| r.collection_event_id)))
        .all(db)
        .await?
        .into_iter()
        .map(|e| (e.id, e))
        .collect();
    let sample_map: HashMap<Uuid, samples::Model> = samples::Entity::find()
        .filter(samples::Column::Id.is_in(collect(|r| r.sample_id)))
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.id, s))
        .collect();
    let site_names: HashMap<Uuid, String> = sites::Entity::find()
        .filter(
            sites::Column::Id.is_in(
                deployment_map
                    .values()
                    .map(|d| d.site_id)
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>(),
            ),
        )
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.id, s.name))
        .collect();

    let stream_ids: Vec<Uuid> = groups.keys().copied().collect();
    let mut receipts = fetch_covering_receipts(db, &stream_ids, time).await?;
    let mut holds_by_stream = fetch_stream_holds(db, &stream_ids, time).await?;
    let mut holds_by_slot = fetch_slot_holds(db, rows, time).await?;
    let mut pins = load_pins(db, &stream_ids, time).await?;
    let value_arrivals = load_value_arrivals(db, &stream_ids, time).await?;
    let run_sources = fetch_run_sources(db, rows).await?;
    let (calculations, formula_versions) = fetch_calculations(db, rows).await?;
    let links = fetch_formula_links(db, rows).await?;
    let served = fetch_served_values(db, rows, &links, time).await?;

    let mut records = Vec::with_capacity(groups.len());
    for (stream_id, group) in &groups {
        let stream = streams
            .get(stream_id)
            .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

        let receipt = receipts.remove(stream_id);
        let mut holds = holds_by_stream.remove(stream_id).unwrap_or_default();
        if let (Some(site_id), Some(parameter_id)) = (group[0].site_id, group[0].parameter_id) {
            holds.extend(
                holds_by_slot
                    .remove(&(site_id, parameter_id))
                    .unwrap_or_default(),
            );
        }

        let readings_out: Vec<ReadingFacet> = group
            .iter()
            .map(|r| ReadingFacet {
                replicate_index: r.replicate_index,
                raw_value: r.raw_value,
                calibrated_value: r.calibrated_value,
                measurement_type: r.measurement_type.clone(),
                is_flagged: r.is_flagged.unwrap_or(false),
                flag_reason: r.flag_reason.clone(),
                withdrawn_at: r.withdrawn_at,
                unverified: r.unverified.unwrap_or(false),
                withdrawn_reason: r.withdrawn_reason.clone(),
                ingested_at: r.ingested_at,
                value_arrived_at: value_arrivals
                    .get(&(*stream_id, r.replicate_index))
                    .copied()
                    .or(r.ingested_at),
                provenance_kind: r.provenance_kind.clone(),
                calibration: r.calibration_id.and_then(|id| {
                    calibration_map.get(&id).map(|c| CalibrationRef {
                        id: c.id,
                        slope: c.slope,
                        intercept: c.intercept,
                        valid_from: c.valid_from,
                        valid_until: c.valid_until,
                        retired_at: c.retired_at,
                    })
                }),
                standard_curve: r.standard_curve_id.and_then(|id| {
                    curve_map.get(&id).map(|c| CurveRef {
                        id: c.id,
                        sensor_id: c.sensor_id,
                        name: c.name.clone(),
                        slope: c.slope,
                        intercept: c.intercept,
                        retired_at: c.retired_at,
                    })
                }),
            })
            .collect();

        let sensor = group
            .iter()
            .find_map(|r| r.sensor_id)
            .and_then(|id| sensor_map.get(&id))
            .map(|s| SensorRef {
                id: s.id,
                serial_number: s.serial_number.clone(),
                name: s.name.clone(),
                manufacturer: s.manufacturer.clone(),
                model: s.model.clone(),
            });
        let deployment = group
            .iter()
            .find_map(|r| r.deployment_id)
            .and_then(|id| deployment_map.get(&id))
            .map(|d| DeploymentRef {
                id: d.id,
                site_id: d.site_id,
                site_name: site_names.get(&d.site_id).cloned(),
                deployed_from: d.deployed_from,
                deployed_until: d.deployed_until,
            });
        let event = group
            .iter()
            .find_map(|r| r.collection_event_id)
            .and_then(|id| event_map.get(&id))
            .map(|e| EventRef {
                id: e.id,
                collected_at: e.collected_at,
                source: e.source.clone(),
                created_by: e.created_by.clone(),
            });
        // The story of a measurement lives on the reading, so a group with no statistics row
        // still has one; the sample adds the estimator its stored sd was computed with.
        let sample = group
            .iter()
            .find_map(|r| r.sample_id)
            .and_then(|id| sample_map.get(&id));
        let blob = group.iter().find_map(|r| r.provenance.clone());
        let entered_by = group.iter().find_map(|r| r.created_by.clone());
        let computation = if blob.is_some() || entered_by.is_some() || sample.is_some() {
            let run_source = run_id_of(blob.as_ref()).and_then(|id| run_sources.get(&id).cloned());
            Some(ComputationInfo {
                sample_id: sample.map(|s| s.id),
                created_by: entered_by,
                label: group.iter().find_map(|r| r.label.clone()),
                notes: group.iter().find_map(|r| r.notes.clone()),
                provenance: blob,
                run_source,
                sd_estimator: sample.map(|s| s.sd_estimator.clone()),
                sd_estimator_source: sample.map(|s| s.sd_estimator_source.clone()),
                n: sample.map(|s| s.n),
                mean: sample.and_then(|s| s.mean),
                stdev: sample.and_then(|s| s.stdev),
                stdev_sample: sample.and_then(|s| s.stdev_sample),
                stdev_population: sample.and_then(|s| s.stdev_population),
                median: sample.and_then(|s| s.median),
                min: sample.and_then(|s| s.min_value),
                max: sample.and_then(|s| s.max_value),
            })
        } else {
            None
        };

        // A derived value's calculation, with the version this group's rows name where they
        // name one at all.
        let calculation = group
            .iter()
            .filter(|r| r.measurement_type.as_deref() == Some("derived"))
            .find_map(|r| r.parameter_id)
            .and_then(|parameter_id| calculations.get(&parameter_id).cloned())
            .map(|mut calc| {
                if let Some((version_id, (version_no, formula, content_hash))) = group
                    .iter()
                    .find_map(|r| r.derived_version_id)
                    .and_then(|id| formula_versions.get(&id).map(|v| (id, v)))
                {
                    calc.version_id = Some(version_id);
                    calc.version_no = Some(*version_no);
                    calc.formula = Some(formula.clone());
                    calc.content_hash = Some(content_hash.clone());
                }
                calc
            });

        let indexes: Vec<i16> = group.iter().map(|r| r.replicate_index).collect();
        let (inputs, consumers) = match (group[0].site_id, group[0].parameter_id) {
            (Some(site_id), Some(parameter_id)) => (
                links.inputs_of(parameter_id, &indexes, &served, site_id),
                links.consumers_of(parameter_id, &indexes, &served, site_id),
            ),
            _ => (Vec::new(), Vec::new()),
        };

        records.push(ProvenanceRecord {
            origin: OriginInfo {
                stream_id: *stream_id,
                source_system: stream.source_system.clone(),
                source_key: stream.source_key.clone(),
                source_name: stream.source_name.clone(),
                classification: classify_source(&stream.source_system).to_string(),
                paired_at: stream.paired_at.map(|t| t.with_timezone(&Utc)),
                ingested_at: group.iter().filter_map(|r| r.ingested_at).max(),
                value_arrived_at: group
                    .iter()
                    .filter_map(|r| {
                        value_arrivals
                            .get(&(*stream_id, r.replicate_index))
                            .copied()
                            .or(r.ingested_at)
                    })
                    .max(),
                receipt,
            },
            readings: readings_out,
            chain: ChainInfo {
                sensor,
                deployment,
                pins: pins.remove(stream_id).unwrap_or_default(),
            },
            event,
            computation,
            calculation,
            inputs,
            consumers,
            holds,
        });
    }

    Ok(records)
}

/// Every record at a collection event, keyed by the stream that serves it.
pub async fn records_for_event(
    db: &sea_orm::DatabaseConnection,
    event_id: Uuid,
    collected_at: DateTime<Utc>,
) -> AppResult<HashMap<Uuid, ProvenanceRecord>> {
    let rows: Vec<RawRow> = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT {ROW_COLUMNS} FROM readings WHERE collection_event_id = $1 \
                 ORDER BY stream_id, replicate_index"
            ),
            [event_id.into()],
        ))
        .await?
        .iter()
        .map(|row| RawRow::from_query_result(row, ""))
        .collect::<Result<_, _>>()?;
    Ok(assemble_records(db, &rows, collected_at)
        .await?
        .into_iter()
        .map(|r| (r.origin.stream_id, r))
        .collect())
}

pub fn run_id_of(blob: Option<&serde_json::Value>) -> Option<Uuid> {
    blob?
        .get("run_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
}

fn sanitize_cadence(value: &str) -> AppResult<&str> {
    match value {
        "spot" | "derived" => Ok(value),
        _ => Err(AppError::BadRequest(format!(
            "measurement_type must be continuous, spot or derived, got '{value}'"
        ))),
    }
}

/// The latest windowed-ingest pass covering the instant, per stream.
async fn fetch_covering_receipts(
    db: &sea_orm::DatabaseConnection,
    stream_ids: &[Uuid],
    time: DateTime<Utc>,
) -> AppResult<HashMap<Uuid, ReceiptSummary>> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT DISTINCT ON (stream_id) stream_id, id, at, window_from, window_to, \
                    submitted, new_rows, changed, unchanged, withdrawn, rejected_total, braked \
             FROM ingest_receipts \
             WHERE stream_id = ANY($1) AND window_from <= $2 AND window_to >= $2 \
             ORDER BY stream_id, at DESC",
            [stream_ids.to_vec().into(), time.into()],
        ))
        .await?;
    let mut out = HashMap::new();
    for row in rows
        .iter()
        .map(|r| CoveringReceipt::from_query_result(r, ""))
    {
        let row = row?;
        out.insert(
            row.stream_id,
            ReceiptSummary {
                id: row.id,
                at: row.at.map_or(time, |t| t.with_timezone(&Utc)),
                window_from: row.window_from.map(|t| t.with_timezone(&Utc)),
                window_to: row.window_to.map(|t| t.with_timezone(&Utc)),
                submitted: row.submitted,
                new_rows: row.new_rows,
                changed: row.changed,
                unchanged: row.unchanged,
                withdrawn: row.withdrawn,
                rejected_total: row.rejected_total,
                braked: row.braked,
            },
        );
    }
    Ok(out)
}

#[derive(FromQueryResult)]
struct CoveringReceipt {
    stream_id: Uuid,
    id: Uuid,
    at: Option<DateTime<chrono::FixedOffset>>,
    window_from: Option<DateTime<chrono::FixedOffset>>,
    window_to: Option<DateTime<chrono::FixedOffset>>,
    submitted: i32,
    new_rows: i32,
    changed: i32,
    unchanged: i32,
    withdrawn: i32,
    rejected_total: i32,
    braked: bool,
}

/// A hold as its queries select it, with the key column each of the two shapes carries.
#[derive(FromQueryResult)]
struct HoldRow {
    stream_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
    id: Uuid,
    kind: String,
    status: String,
    created_at: DateTime<chrono::FixedOffset>,
}

impl From<&HoldRow> for HoldRef {
    fn from(row: &HoldRow) -> Self {
        Self {
            id: row.id,
            kind: row.kind.clone(),
            status: row.status.clone(),
            created_at: row.created_at.with_timezone(&Utc),
        }
    }
}

/// Replicate-statistics holds keyed by stream at the instant. Terminal holds are left out.
async fn fetch_stream_holds(
    db: &sea_orm::DatabaseConnection,
    stream_ids: &[Uuid],
    time: DateTime<Utc>,
) -> AppResult<HashMap<Uuid, Vec<HoldRef>>> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT stream_id, NULL::uuid AS parameter_id, id, kind, status, created_at \
             FROM replicate_audit_holds \
             WHERE stream_id = ANY($1) AND group_time = $2 \
               AND status IN ('pending', 'deferred', 'acknowledged') \
             ORDER BY created_at DESC",
            [stream_ids.to_vec().into(), time.into()],
        ))
        .await?;
    let mut out: HashMap<Uuid, Vec<HoldRef>> = HashMap::new();
    for row in rows.iter().map(|r| HoldRow::from_query_result(r, "")) {
        let row = row?;
        if let Some(stream_id) = row.stream_id {
            out.entry(stream_id).or_default().push((&row).into());
        }
    }
    Ok(out)
}

/// Event-audit findings and reconciliation holds keyed by (site, parameter) at the instant.
async fn fetch_slot_holds(
    db: &sea_orm::DatabaseConnection,
    rows: &[RawRow],
    time: DateTime<Utc>,
) -> AppResult<HashMap<(Uuid, Uuid), Vec<HoldRef>>> {
    let mut by_site: BTreeMap<Uuid, HashSet<Uuid>> = BTreeMap::new();
    for r in rows {
        if let (Some(site_id), Some(parameter_id)) = (r.site_id, r.parameter_id) {
            by_site.entry(site_id).or_default().insert(parameter_id);
        }
    }
    let mut out: HashMap<(Uuid, Uuid), Vec<HoldRef>> = HashMap::new();
    for (site_id, parameter_ids) in by_site {
        let found = db
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT NULL::uuid AS stream_id, parameter_id, id, kind, status, created_at \
                 FROM replicate_audit_holds \
                 WHERE stream_id IS NULL AND site_id = $1 AND parameter_id = ANY($2) \
                   AND group_time = $3 AND status IN ('pending', 'deferred', 'acknowledged') \
                 ORDER BY created_at DESC",
                [
                    site_id.into(),
                    parameter_ids.into_iter().collect::<Vec<_>>().into(),
                    time.into(),
                ],
            ))
            .await?;
        for row in found.iter().map(|r| HoldRow::from_query_result(r, "")) {
            let row = row?;
            if let Some(parameter_id) = row.parameter_id {
                out.entry((site_id, parameter_id))
                    .or_default()
                    .push((&row).into());
            }
        }
    }
    Ok(out)
}

/// The minting path of every tool run the rows' provenance blobs name.
async fn fetch_run_sources(
    db: &sea_orm::DatabaseConnection,
    rows: &[RawRow],
) -> AppResult<HashMap<Uuid, String>> {
    let run_ids: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| run_id_of(r.provenance.as_ref()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if run_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let found = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, source FROM tool_runs WHERE id = ANY($1)",
            [run_ids.into()],
        ))
        .await?;
    let mut out = HashMap::new();
    for row in found.iter().map(|r| RunSourceRow::from_query_result(r, "")) {
        let row = row?;
        if let Some(source) = row.source {
            out.insert(row.id, source);
        }
    }
    Ok(out)
}

#[derive(FromQueryResult)]
struct RunSourceRow {
    id: Uuid,
    source: Option<String>,
}

/// The calculation behind every derived row, keyed by the output parameter it writes, and the
/// versions the stored values name, keyed by their own ids. A row naming no version keeps the
/// definition and reports no formula, because the text that produced it is not recoverable (M134).
type FormulaVersion = (i32, String, String);

async fn fetch_calculations(
    db: &sea_orm::DatabaseConnection,
    rows: &[RawRow],
) -> AppResult<(
    HashMap<Uuid, CalculationInfo>,
    HashMap<Uuid, FormulaVersion>,
)> {
    let parameter_ids: Vec<Uuid> = rows
        .iter()
        .filter(|r| r.measurement_type.as_deref() == Some("derived"))
        .filter_map(|r| r.parameter_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if parameter_ids.is_empty() {
        return Ok((HashMap::new(), HashMap::new()));
    }
    let definitions = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT d.id, d.code, d.name, d.output_parameter_id, \
                    (SELECT max(v.version_no) FROM derived_parameter_definition_versions v \
                      WHERE v.definition_id = d.id) AS active_version_no \
               FROM calculation_formulas d \
              WHERE d.output_parameter_id = ANY($1)",
            [parameter_ids.into()],
        ))
        .await?;
    let mut by_parameter: HashMap<Uuid, CalculationInfo> = HashMap::new();
    for row in definitions
        .iter()
        .map(|r| DefinitionRow::from_query_result(r, ""))
    {
        let row = row?;
        let Some(output) = row.output_parameter_id else {
            continue;
        };
        by_parameter.insert(
            output,
            CalculationInfo {
                definition_id: row.id,
                code: row.code,
                name: row.name,
                version_id: None,
                version_no: None,
                formula: None,
                content_hash: None,
                active_version_no: row.active_version_no,
            },
        );
    }

    let version_ids: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| r.derived_version_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if version_ids.is_empty() {
        return Ok((by_parameter, HashMap::new()));
    }
    let found = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, version_no, formula, content_hash \
               FROM derived_parameter_definition_versions WHERE id = ANY($1)",
            [version_ids.into()],
        ))
        .await?;
    let mut versions = HashMap::new();
    for row in found.iter().map(|r| VersionRow::from_query_result(r, "")) {
        let row = row?;
        versions.insert(row.id, (row.version_no, row.formula, row.content_hash));
    }
    Ok((by_parameter, versions))
}

#[derive(FromQueryResult)]
struct DefinitionRow {
    id: Uuid,
    code: String,
    name: String,
    output_parameter_id: Option<Uuid>,
    active_version_no: Option<i32>,
}

#[derive(FromQueryResult)]
struct VersionRow {
    id: Uuid,
    version_no: i32,
    formula: String,
    content_hash: String,
}

/// A formula and what it reads, as the sources table records it today. Sources are not versioned,
/// so a value made by an older version is joined to the definition's current inputs.
#[derive(Debug, Clone)]
struct FormulaLink {
    definition_id: Uuid,
    code: String,
    name: String,
    per_replicate: Option<String>,
    output_parameter_id: Option<Uuid>,
    output_parameter_code: Option<String>,
    calculation: Option<String>,
    enabled: bool,
    sources: Vec<SourceLink>,
}

#[derive(Debug, Clone)]
struct SourceLink {
    variable_name: String,
    parameter_id: Option<Uuid>,
    parameter_code: Option<String>,
    site_property: Option<String>,
}

/// The formulas one hop from the rows' parameters: those producing one, and those reading one.
#[derive(Debug, Default)]
struct FormulaLinks {
    formulas: Vec<FormulaLink>,
}

/// What a parameter's slot serves at one instant: each replicate's value, the family's mean where
/// the trigger derived one, and the value a scalar read of the slot receives.
#[derive(Debug, Default)]
struct ServedSlot {
    by_index: BTreeMap<i16, f64>,
    mean: Option<f64>,
    scalar: Option<f64>,
}

/// Served values keyed by `(site_id, parameter_id)`, plus the site rows' numeric columns.
#[derive(Debug, Default)]
struct ServedValues {
    slots: HashMap<(Uuid, Uuid), ServedSlot>,
    site_properties: HashMap<Uuid, HashMap<String, f64>>,
}

impl ServedValues {
    fn scalar(&self, site_id: Uuid, parameter_id: Uuid) -> (Option<f64>, &'static str) {
        match self.slots.get(&(site_id, parameter_id)) {
            Some(slot) if slot.mean.is_some() => (slot.mean, "mean"),
            Some(slot) if slot.scalar.is_some() => (slot.scalar, "reading"),
            _ => (None, "missing"),
        }
    }

    fn at_index(
        &self,
        site_id: Uuid,
        parameter_id: Uuid,
        index: i16,
    ) -> (Option<f64>, &'static str) {
        match self
            .slots
            .get(&(site_id, parameter_id))
            .and_then(|slot| slot.by_index.get(&index).copied())
        {
            Some(value) => (Some(value), "replicate"),
            None => (None, "missing"),
        }
    }

    fn site_property(&self, site_id: Uuid, column: &str) -> (Option<f64>, &'static str) {
        match self
            .site_properties
            .get(&site_id)
            .and_then(|row| row.get(column).copied())
        {
            Some(value) => (Some(value), "site"),
            None => (None, "missing"),
        }
    }
}

impl FormulaLinks {
    /// Every parameter a lookup at the instant has to serve: the sources of the producers and the
    /// outputs of the consumers.
    fn parameters_to_serve(&self) -> Vec<Uuid> {
        self.formulas
            .iter()
            .flat_map(|f| {
                f.sources
                    .iter()
                    .filter_map(|s| s.parameter_id)
                    .chain(f.output_parameter_id)
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    fn site_properties_to_serve(&self) -> Vec<String> {
        self.formulas
            .iter()
            .flat_map(|f| f.sources.iter().filter_map(|s| s.site_property.clone()))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    /// The inputs of every formula producing `parameter_id`, read at the record's indexes.
    fn inputs_of(
        &self,
        parameter_id: Uuid,
        indexes: &[i16],
        served: &ServedValues,
        site_id: Uuid,
    ) -> Vec<InputRef> {
        let mut out = Vec::new();
        for formula in self
            .formulas
            .iter()
            .filter(|f| f.output_parameter_id == Some(parameter_id))
        {
            for source in &formula.sources {
                let make = |replicate_index, (value, served_as): (Option<f64>, &str)| InputRef {
                    definition_id: formula.definition_id,
                    formula_code: formula.code.clone(),
                    variable_name: source.variable_name.clone(),
                    parameter_id: source.parameter_id,
                    parameter_code: source.parameter_code.clone(),
                    site_property: source.site_property.clone(),
                    replicate_index,
                    served_as: served_as.to_string(),
                    value,
                };
                match (source.parameter_id, &source.site_property) {
                    (Some(input), _)
                        if formula.per_replicate.as_deref() == Some(&source.variable_name) =>
                    {
                        for &index in indexes {
                            out.push(make(Some(index), served.at_index(site_id, input, index)));
                        }
                    }
                    (Some(input), _) => out.push(make(None, served.scalar(site_id, input))),
                    (None, Some(column)) => {
                        out.push(make(None, served.site_property(site_id, column)));
                    }
                    (None, None) => {}
                }
            }
        }
        out
    }

    /// Every enabled formula reading `parameter_id`, with its output at the record's indexes.
    fn consumers_of(
        &self,
        parameter_id: Uuid,
        indexes: &[i16],
        served: &ServedValues,
        site_id: Uuid,
    ) -> Vec<ConsumerRef> {
        let mut out = Vec::new();
        for formula in self.formulas.iter().filter(|f| f.enabled) {
            for source in formula
                .sources
                .iter()
                .filter(|s| s.parameter_id == Some(parameter_id))
            {
                let make = |replicate_index, value| ConsumerRef {
                    definition_id: formula.definition_id,
                    formula_code: formula.code.clone(),
                    formula_name: formula.name.clone(),
                    calculation: formula.calculation.clone(),
                    variable_name: source.variable_name.clone(),
                    output_parameter_id: formula.output_parameter_id,
                    output_parameter_code: formula.output_parameter_code.clone(),
                    replicate_index,
                    value,
                };
                let per_replicate = formula.per_replicate.as_deref() == Some(&source.variable_name);
                match (formula.output_parameter_id, per_replicate) {
                    (Some(output), true) => {
                        for &index in indexes {
                            out.push(make(Some(index), served.at_index(site_id, output, index).0));
                        }
                    }
                    (Some(output), false) => out.push(make(None, served.scalar(site_id, output).0)),
                    (None, _) => out.push(make(None, None)),
                }
            }
        }
        out
    }
}

/// The formulas one hop from the rows' parameters, with their sources. A formula under a disabled
/// calculation is kept as a producer (the value it made is still its) and dropped as a consumer.
async fn fetch_formula_links(
    db: &sea_orm::DatabaseConnection,
    rows: &[RawRow],
) -> AppResult<FormulaLinks> {
    let parameter_ids: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| r.parameter_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if parameter_ids.is_empty() {
        return Ok(FormulaLinks::default());
    }
    let found = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT d.id, d.code, d.name, d.per_replicate, d.output_parameter_id, \
                    p.code AS output_parameter_code, s.name AS calculation, \
                    COALESCE(s.enabled, true) AS enabled \
               FROM calculation_formulas d \
               LEFT JOIN tool_scripts s ON s.id = d.tool_script_id \
               LEFT JOIN parameters p ON p.id = d.output_parameter_id \
              WHERE d.output_parameter_id = ANY($1) \
                 OR d.id IN (SELECT derived_definition_id FROM derived_parameter_sources \
                              WHERE parameter_id = ANY($1)) \
              ORDER BY d.ordinal, d.code",
            [parameter_ids.into()],
        ))
        .await?;
    let mut formulas: Vec<FormulaLink> = Vec::with_capacity(found.len());
    for row in found.iter().map(|r| LinkRow::from_query_result(r, "")) {
        let row = row?;
        formulas.push(FormulaLink {
            definition_id: row.id,
            code: row.code,
            name: row.name,
            per_replicate: row.per_replicate,
            output_parameter_id: row.output_parameter_id,
            output_parameter_code: row.output_parameter_code,
            calculation: row.calculation,
            enabled: row.enabled,
            sources: Vec::new(),
        });
    }
    if formulas.is_empty() {
        return Ok(FormulaLinks::default());
    }
    let definition_ids: Vec<Uuid> = formulas.iter().map(|f| f.definition_id).collect();
    let sources = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT ds.derived_definition_id, ds.variable_name, ds.parameter_id, \
                    ds.site_property, p.code AS parameter_code \
               FROM derived_parameter_sources ds \
               LEFT JOIN parameters p ON p.id = ds.parameter_id \
              WHERE ds.derived_definition_id = ANY($1) \
              ORDER BY ds.variable_name",
            [definition_ids.into()],
        ))
        .await?;
    let mut by_definition: HashMap<Uuid, Vec<SourceLink>> = HashMap::new();
    for row in sources.iter().map(|r| SourceRow::from_query_result(r, "")) {
        let row = row?;
        by_definition
            .entry(row.derived_definition_id)
            .or_default()
            .push(SourceLink {
                variable_name: row.variable_name,
                parameter_id: row.parameter_id,
                parameter_code: row.parameter_code,
                site_property: row.site_property,
            });
    }
    for formula in &mut formulas {
        formula.sources = by_definition
            .remove(&formula.definition_id)
            .unwrap_or_default();
    }
    Ok(FormulaLinks { formulas })
}

#[derive(FromQueryResult)]
struct LinkRow {
    id: Uuid,
    code: String,
    name: String,
    per_replicate: Option<String>,
    output_parameter_id: Option<Uuid>,
    output_parameter_code: Option<String>,
    calculation: Option<String>,
    enabled: bool,
}

#[derive(FromQueryResult)]
struct SourceRow {
    derived_definition_id: Uuid,
    variable_name: String,
    parameter_id: Option<Uuid>,
    site_property: Option<String>,
    parameter_code: Option<String>,
}

/// What the linked parameters serve at the instant, at every site the rows name. A scalar read
/// of a slot receives the family's mean where the trigger derived one, else the lowest live
/// replicate's value, which is the serving contract's spot arm.
async fn fetch_served_values(
    db: &sea_orm::DatabaseConnection,
    rows: &[RawRow],
    links: &FormulaLinks,
    time: DateTime<Utc>,
) -> AppResult<ServedValues> {
    let mut served = ServedValues::default();
    let site_ids: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| r.site_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let parameter_ids = links.parameters_to_serve();
    if site_ids.is_empty() || parameter_ids.is_empty() {
        return Ok(served);
    }
    let found = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT r.site_id, r.parameter_id, r.replicate_index, \
                    COALESCE(r.calibrated_value, r.raw_value) AS value, s.mean, \
                    (r.is_flagged IS NOT TRUE AND r.withdrawn_at IS NULL) AS live \
               FROM readings r \
               LEFT JOIN samples s ON s.id = r.sample_id \
              WHERE r.site_id = ANY($1) AND r.parameter_id = ANY($2) AND r.time = $3 \
              ORDER BY r.site_id, r.parameter_id, r.replicate_index",
            [site_ids.clone().into(), parameter_ids.into(), time.into()],
        ))
        .await?;
    for row in found.iter().map(|r| ServedRow::from_query_result(r, "")) {
        let row = row?;
        let slot = served
            .slots
            .entry((row.site_id, row.parameter_id))
            .or_default();
        slot.by_index.insert(row.replicate_index, row.value);
        slot.mean = slot.mean.or(row.mean);
        if row.live && slot.scalar.is_none() {
            slot.scalar = Some(row.value);
        }
    }

    let columns = links.site_properties_to_serve();
    if columns.is_empty() {
        return Ok(served);
    }
    let sites = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, to_jsonb(sites) AS row FROM sites WHERE id = ANY($1)",
            [site_ids.into()],
        ))
        .await?;
    for row in sites.iter().map(|r| SiteRow::from_query_result(r, "")) {
        let row = row?;
        let values: HashMap<String, f64> = columns
            .iter()
            .filter_map(|c| {
                row.row
                    .get(c)
                    .and_then(serde_json::Value::as_f64)
                    .map(|v| (c.clone(), v))
            })
            .collect();
        served.site_properties.insert(row.id, values);
    }
    Ok(served)
}

#[derive(FromQueryResult)]
struct ServedRow {
    site_id: Uuid,
    parameter_id: Uuid,
    replicate_index: i16,
    value: f64,
    mean: Option<f64>,
    live: bool,
}

#[derive(FromQueryResult)]
struct SiteRow {
    id: Uuid,
    row: serde_json::Value,
}

/// The slot's code, name, unit and declared precision. The site's own configuration wins over the
/// catalog default, which is what makes the number on screen readable in the unit it was served in.
async fn slot_identity(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> AppResult<Option<(String, String, Option<String>, Option<i16>)>> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT p.code, p.name, COALESCE(sp.display_units, p.default_units) AS units, \
                    sp.decimal_places \
             FROM parameters p \
             LEFT JOIN site_parameters sp ON sp.parameter_id = p.id AND sp.site_id = $2 \
             WHERE p.id = $1 LIMIT 1",
            [parameter_id.into(), site_id.into()],
        ))
        .await?;
    let Some(row) = row else { return Ok(None) };
    let row = SlotRow::from_query_result(&row, "")?;
    Ok(Some((row.code, row.name, row.units, row.decimal_places)))
}

#[derive(FromQueryResult)]
struct SlotRow {
    code: String,
    name: String,
    units: Option<String>,
    decimal_places: Option<i16>,
}

#[cfg(test)]
#[path = "tests/provenance.rs"]
mod tests;
