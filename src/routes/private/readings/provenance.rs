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
    pub site_id: Option<Uuid>,
    pub parameter_id: Option<Uuid>,
    /// What the instant measured, named. The record was serving bare numbers under a bare uuid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameter_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameter_name: Option<String>,
    /// The slot's unit when it declares one, the catalog default otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
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
    pub event: Option<EventRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub computation: Option<ComputationInfo>,
    pub holds: Vec<HoldRef>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OriginInfo {
    pub stream_id: Uuid,
    pub source_system: String,
    pub source_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    /// 'sync' | 'manual' | 'csv' | 'api', from the stream's source system.
    pub classification: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paired_at: Option<DateTime<Utc>>,
    /// Latest first-arrival stamp in the replicate group: when these rows first existed. NULL
    /// means they predate tracking. Nothing moves it, so a corrected row still reports its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingested_at: Option<DateTime<Utc>>,
    /// When the value the group currently serves arrived: the latest live value correction's `at`
    /// where one exists, and the first arrival otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_arrived_at: Option<DateTime<Utc>>,
    /// The latest windowed-ingest pass whose claimed window covers the instant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<ReceiptSummary>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReceiptSummary {
    pub id: Uuid,
    pub at: DateTime<Utc>,
    pub window_from: Option<DateTime<Utc>>,
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
    pub calibrated_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measurement_type: Option<String>,
    pub is_flagged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flag_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub withdrawn_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub withdrawn_reason: Option<String>,
    /// A pending entry: stored and shown here, never published (Q18, Q21).
    pub unverified: bool,
    /// When this row first existed. Nothing moves it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingested_at: Option<DateTime<Utc>>,
    /// When the value this row currently serves arrived: its latest live value correction's `at`,
    /// else its first arrival.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_arrived_at: Option<DateTime<Utc>>,
    /// Where this value came from, one of `PROVENANCE_KINDS`. A `sync` or `derived` row's story is
    /// resolved from the stream, the receipt and the definition; the others carry a stored blob.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibration: Option<CalibrationRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standard_curve: Option<CurveRef>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CalibrationRef {
    pub id: Uuid,
    pub slope: f64,
    pub intercept: f64,
    pub valid_from: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CurveRef {
    pub id: Uuid,
    /// The lab instrument the curve belongs to, which is where its record lives.
    pub sensor_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub slope: f64,
    pub intercept: f64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChainInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sensor: Option<SensorRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
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
    for r in &rows {
        out.insert(
            (r.try_get("", "stream_id")?, r.try_get("", "replicate_index")?),
            r.try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "at")?
                .with_timezone(&Utc),
        );
    }
    Ok(out)
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
    for r in &rows {
        let stream_id: Uuid = r.try_get("", "stream_id")?;
        out.entry(stream_id).or_default().push(PinRef {
            decision_id: r.try_get("", "id")?,
            kind: r.try_get("", "kind")?,
            replicate_index: r.try_get("", "replicate_index")?,
            target: r.try_get("", "new")?,
            actor: r.try_get("", "actor")?,
            at: r
                .try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "at")?
                .with_timezone(&Utc),
            reason: r.try_get("", "reason")?,
            set_id: r.try_get("", "set_id")?,
        });
    }
    Ok(out)
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PinRef {
    pub decision_id: Uuid,
    /// `instrument_pin` | `calibration_pin`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replicate_index: Option<i16>,
    #[schema(value_type = Object)]
    pub target: serde_json::Value,
    pub actor: String,
    pub at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SensorRef {
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeploymentRef {
    pub id: Uuid,
    pub site_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_name: Option<String>,
    pub deployed_from: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployed_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EventRef {
    pub id: Uuid,
    pub collected_at: DateTime<Utc>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ComputationInfo {
    /// The statistics row, when the instant carries two or more replicates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// The server-built tool-run blob stored on the reading, verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<serde_json::Value>,
    /// The run's minting path: 'interactive' | 'csv_import' | 'chain'.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_source: Option<String>,
    /// Which divisor this group's served standard deviation uses ('sample' = n-1, 'population' =
    /// n) and what chose it. `sd_estimator_source` 'default' means nothing declared one, so the
    /// number is served under a convention nobody stated. Absent on a single measurement, which
    /// has no standard deviation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sd_estimator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sd_estimator_source: Option<String>,
    /// The group's statistics, the numbers the chart plotted and drew its bar from. Without these
    /// the record shows the replicates and a sentence about the divisor, and never what was served.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdev: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdev_sample: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdev_population: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct HoldRef {
    pub id: Uuid,
    pub kind: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

/// The stream's origin class, from the writer-side source-system set in
/// `collection_events::attach`.
/// How a reading reached the store, from the stream it arrived on: `manual`, `csv`, `api` or
/// `sync`. One definition, so every surface naming an origin names the same thing.
pub fn classify_source(source_system: &str) -> &'static str {
    match source_system {
        "grab_sample" => "manual",
        "csv" | "csv_import" => "csv",
        "api" => "api",
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

/// Whether a reading's provenance is untold: nothing on the row records where it came from, and
/// nothing it points at can be asked.
///
/// The kinds that store a blob are only as good as the blob; `sync` and `derived` are resolved, so
/// what they need is a referent that answers; `manual` is complete on its own (the person, the
/// time and the check are on the row); `migration` is the name for an origin nobody recorded, so
/// it is untold by definition and is what the count is mostly about.
#[must_use]
pub fn provenance_untold(kind: Option<&str>, has_blob: bool, referent_resolves: bool) -> bool {
    match kind {
        None | Some("migration") => true,
        Some("tool_run" | "chain" | "csv_import") => !has_blob,
        Some("derived") => !referent_resolves,
        _ => false,
    }
}

/// The readings [`provenance_untold`] holds, as one statement. Report-only: which side is wrong is
/// a question about the writer, not something a sweep may decide.
#[must_use]
pub fn untold_rows_sql() -> String {
    "SELECT r.stream_id, r.time, r.replicate_index, r.provenance_kind
       FROM readings r
      WHERE r.provenance_kind IS NULL
         OR r.provenance_kind = 'migration'
         OR (r.provenance_kind IN ('tool_run', 'chain', 'csv_import') AND r.provenance IS NULL)
         OR (r.provenance_kind = 'derived' AND NOT EXISTS (
                SELECT 1 FROM derived_parameter_definitions d
                 WHERE d.output_parameter_id = r.parameter_id))"
        .to_string()
}

/// How many readings say nothing about where they came from. The janitor reports it; nothing
/// repairs it.
pub async fn untold_count<C: sea_orm::ConnectionTrait>(conn: &C) -> crate::error::AppResult<i64> {
    let row = conn
        .query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT count(*)::bigint AS n FROM ({}) untold",
                untold_rows_sql()
            ),
        ))
        .await?
        .ok_or_else(|| {
            crate::error::AppError::Internal(
                "counting untold provenance returned no row".to_string(),
            )
        })?;
    Ok(row.try_get("", "n")?)
}

#[derive(Debug, FromQueryResult)]
pub struct RawRow {
    stream_id: Uuid,
    replicate_index: i16,
    site_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
    raw_value: f64,
    calibrated_value: Option<f64>,
    sensor_id: Option<Uuid>,
    calibration_id: Option<Uuid>,
    standard_curve_id: Option<Uuid>,
    deployment_id: Option<Uuid>,
    measurement_type: Option<String>,
    is_flagged: Option<bool>,
    flag_reason: Option<String>,
    sample_id: Option<Uuid>,
    collection_event_id: Option<Uuid>,
    withdrawn_at: Option<DateTime<Utc>>,
    unverified: Option<bool>,
    withdrawn_reason: Option<String>,
    ingested_at: Option<DateTime<Utc>>,
    provenance_kind: Option<String>,
    provenance: Option<serde_json::Value>,
    label: Option<String>,
    notes: Option<String>,
    created_by: Option<String>,
}

const ROW_COLUMNS: &str = "stream_id, replicate_index, site_id, parameter_id, raw_value, \
     calibrated_value, sensor_id, calibration_id, standard_curve_id, deployment_id, \
     measurement_type, is_flagged, flag_reason, sample_id, collection_event_id, \
     withdrawn_at, withdrawn_reason, unverified, ingested_at, provenance_kind, provenance, \
     label, notes, created_by";

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
            state.db.query_all_raw(stmt).await?
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
            state.db.query_all_raw(stmt).await?
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
                .one(&state.db)
                .await?
                .and_then(|s| s.project_id),
            None => None,
        };
        if !scope.allows_project_opt(project) {
            return Err(AppError::NotFound("No reading at that instant".to_string()));
        }
    }

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
                    })
                }),
                standard_curve: r.standard_curve_id.and_then(|id| {
                    curve_map.get(&id).map(|c| CurveRef {
                        id: c.id,
                        sensor_id: c.sensor_id,
                        name: c.name.clone(),
                        slope: c.slope,
                        intercept: c.intercept,
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

fn run_id_of(blob: Option<&serde_json::Value>) -> Option<Uuid> {
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

fn fixed_at(row: &sea_orm::QueryResult, name: &str) -> Option<DateTime<Utc>> {
    row.try_get::<Option<DateTime<chrono::FixedOffset>>>("", name)
        .ok()
        .flatten()
        .map(|t| t.with_timezone(&Utc))
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
    for row in rows {
        let stream_id: Uuid = row.try_get("", "stream_id")?;
        out.insert(
            stream_id,
            ReceiptSummary {
                id: row.try_get("", "id")?,
                at: fixed_at(&row, "at").unwrap_or(time),
                window_from: fixed_at(&row, "window_from"),
                window_to: fixed_at(&row, "window_to"),
                submitted: row.try_get("", "submitted")?,
                new_rows: row.try_get("", "new_rows")?,
                changed: row.try_get("", "changed")?,
                unchanged: row.try_get("", "unchanged")?,
                withdrawn: row.try_get("", "withdrawn")?,
                rejected_total: row.try_get("", "rejected_total")?,
                braked: row.try_get("", "braked")?,
            },
        );
    }
    Ok(out)
}

fn hold_ref(row: &sea_orm::QueryResult) -> Result<HoldRef, sea_orm::DbErr> {
    let created: DateTime<chrono::FixedOffset> = row.try_get("", "created_at")?;
    Ok(HoldRef {
        id: row.try_get("", "id")?,
        kind: row.try_get("", "kind")?,
        status: row.try_get("", "status")?,
        created_at: created.with_timezone(&Utc),
    })
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
            "SELECT stream_id, id, kind, status, created_at FROM replicate_audit_holds \
             WHERE stream_id = ANY($1) AND group_time = $2 \
               AND status IN ('pending', 'deferred', 'acknowledged') \
             ORDER BY created_at DESC",
            [stream_ids.to_vec().into(), time.into()],
        ))
        .await?;
    let mut out: HashMap<Uuid, Vec<HoldRef>> = HashMap::new();
    for row in rows {
        let stream_id: Uuid = row.try_get("", "stream_id")?;
        out.entry(stream_id).or_default().push(hold_ref(&row)?);
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
                "SELECT parameter_id, id, kind, status, created_at FROM replicate_audit_holds \
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
        for row in found {
            let parameter_id: Uuid = row.try_get("", "parameter_id")?;
            out.entry((site_id, parameter_id))
                .or_default()
                .push(hold_ref(&row)?);
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
    for row in found {
        let id: Uuid = row.try_get("", "id")?;
        if let Some(source) = row.try_get::<Option<String>>("", "source")? {
            out.insert(id, source);
        }
    }
    Ok(out)
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
    Ok(Some((
        row.try_get("", "code")?,
        row.try_get("", "name")?,
        row.try_get("", "units")?,
        row.try_get("", "decimal_places")?,
    )))
}

#[cfg(test)]
mod tests {
    use super::{PROVENANCE_KINDS, provenance_kind_for_run, provenance_kind_for_stream};

    #[test]
    fn test_a_row_that_records_nothing_is_untold() {
        assert!(super::provenance_untold(None, false, false));
        assert!(super::provenance_untold(Some("migration"), true, true));
    }

    #[test]
    fn test_a_stored_kind_is_only_as_good_as_its_blob() {
        assert!(super::provenance_untold(Some("tool_run"), false, true));
        assert!(super::provenance_untold(Some("chain"), false, true));
        assert!(super::provenance_untold(Some("csv_import"), false, true));
        assert!(!super::provenance_untold(Some("tool_run"), true, false));
    }

    #[test]
    fn test_a_resolved_kind_wants_a_referent_that_answers() {
        assert!(super::provenance_untold(Some("derived"), false, false));
        assert!(!super::provenance_untold(Some("derived"), false, true));
        assert!(
            !super::provenance_untold(Some("sync"), false, false),
            "a sync row's story is its stream and the receipt covering the instant, which the FK \
             guarantees is there"
        );
        assert!(
            !super::provenance_untold(Some("manual"), false, false),
            "a hand entry is complete on the row: the person, the time and the check"
        );
        assert!(!super::provenance_untold(Some("batch"), false, false));
    }

    #[test]
    fn test_provenance_kind_for_stream_matches_the_trigger_rule() {
        assert_eq!(
            provenance_kind_for_stream(Some("derived"), Some("cnet")),
            "derived"
        );
        assert_eq!(
            provenance_kind_for_stream(Some("spot"), Some("grab_sample")),
            "manual"
        );
        assert_eq!(provenance_kind_for_stream(None, Some("api")), "batch");
        assert_eq!(
            provenance_kind_for_stream(Some("continuous"), Some("vaisala")),
            "sync"
        );
    }

    #[test]
    fn test_a_stream_that_proves_nothing_is_stamped_migration() {
        assert_eq!(provenance_kind_for_stream(None, None), "migration");
    }

    #[test]
    fn test_provenance_kind_for_run_follows_the_minting_path() {
        assert_eq!(provenance_kind_for_run(None), "manual");
        assert_eq!(provenance_kind_for_run(Some("interactive")), "tool_run");
        assert_eq!(provenance_kind_for_run(Some("chain")), "chain");
        assert_eq!(provenance_kind_for_run(Some("csv_import")), "csv_import");
    }

    #[test]
    fn test_every_kind_a_writer_can_stamp_is_a_declared_kind() {
        for kind in [
            provenance_kind_for_stream(Some("derived"), None),
            provenance_kind_for_stream(None, Some("grab_sample")),
            provenance_kind_for_stream(None, Some("api")),
            provenance_kind_for_stream(None, Some("vaisala")),
            provenance_kind_for_stream(None, None),
            provenance_kind_for_run(None),
            provenance_kind_for_run(Some("interactive")),
            provenance_kind_for_run(Some("chain")),
            provenance_kind_for_run(Some("csv_import")),
        ] {
            assert!(
                PROVENANCE_KINDS.contains(&kind),
                "{kind} is not a declared kind"
            );
        }
    }
}
