use std::collections::{BTreeMap, HashMap, HashSet};

use axum::{
    Json,
    extract::{Query, State},
};
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Statement};
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
    /// Latest arrival stamp in the replicate group. NULL means the rows predate tracking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingested_at: Option<DateTime<Utc>>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingested_at: Option<DateTime<Utc>>,
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
    pub sample_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    /// The server-built tool-run blob stored on the sample, verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<serde_json::Value>,
    /// The run's minting path: 'interactive' | 'csv_import' | 'chain'.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_source: Option<String>,
    /// Which divisor this group's served standard deviation uses ('sample' = n-1, 'population' =
    /// n) and what chose it. `sd_estimator_source` 'default' means nothing declared one, so the
    /// number is served under a convention nobody stated.
    pub sd_estimator: String,
    pub sd_estimator_source: String,
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
fn classify_source(source_system: &str) -> &'static str {
    match source_system {
        "grab_sample" => "manual",
        "csv" | "csv_import" => "csv",
        "api" => "api",
        _ => "sync",
    }
}

struct RawRow {
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
    withdrawn_reason: Option<String>,
    ingested_at: Option<DateTime<Utc>>,
}

const ROW_COLUMNS: &str = "stream_id, replicate_index, site_id, parameter_id, raw_value, \
     calibrated_value, sensor_id, calibration_id, standard_curve_id, deployment_id, \
     measurement_type, is_flagged, flag_reason, sample_id, collection_event_id, \
     withdrawn_at, withdrawn_reason, ingested_at";

fn decode_row(row: &sea_orm::QueryResult) -> Result<RawRow, sea_orm::DbErr> {
    let fixed = |name: &str| -> Option<DateTime<Utc>> {
        row.try_get::<Option<DateTime<chrono::FixedOffset>>>("", name)
            .ok()
            .flatten()
            .map(|t| t.with_timezone(&Utc))
    };
    Ok(RawRow {
        stream_id: row.try_get("", "stream_id")?,
        replicate_index: row.try_get("", "replicate_index")?,
        site_id: row.try_get("", "site_id")?,
        parameter_id: row.try_get("", "parameter_id")?,
        raw_value: row.try_get("", "raw_value")?,
        calibrated_value: row.try_get("", "calibrated_value")?,
        sensor_id: row.try_get("", "sensor_id")?,
        calibration_id: row.try_get("", "calibration_id")?,
        standard_curve_id: row.try_get("", "standard_curve_id")?,
        deployment_id: row.try_get("", "deployment_id")?,
        measurement_type: row.try_get("", "measurement_type")?,
        is_flagged: row.try_get("", "is_flagged")?,
        flag_reason: row.try_get("", "flag_reason")?,
        sample_id: row.try_get("", "sample_id")?,
        collection_event_id: row.try_get("", "collection_event_id")?,
        withdrawn_at: fixed("withdrawn_at"),
        withdrawn_reason: row.try_get("", "withdrawn_reason")?,
        ingested_at: fixed("ingested_at"),
    })
}

/// The assembled record of one measured instant: where it came from, when it arrived, what
/// instrument and corrections produced the stored value, the visit it belongs to, the tool run
/// that computed it, and any review holds touching it. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/readings/provenance",
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
            state.db.query_all(stmt).await?
        }
        (None, Some(site_id), Some(parameter_id)) => {
            let cadence = match q.measurement_type.as_deref() {
                None => String::new(),
                Some("continuous") => {
                    " AND (measurement_type IS NULL OR measurement_type = 'continuous')".into()
                }
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
            state.db.query_all(stmt).await?
        }
        _ => {
            return Err(AppError::BadRequest(
                "Provide either stream_id or both site_id and parameter_id".to_string(),
            ));
        }
    }
    .iter()
    .map(decode_row)
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

    // --- Batch-resolve everything the rows reference ---
    let mut groups: BTreeMap<Uuid, Vec<&RawRow>> = BTreeMap::new();
    for r in &rows {
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
        .all(&state.db)
        .await?
        .into_iter()
        .map(|s| (s.id, s))
        .collect();
    let sensor_map: HashMap<Uuid, sensors::Model> = sensors::Entity::find()
        .filter(sensors::Column::Id.is_in(collect(|r| r.sensor_id)))
        .all(&state.db)
        .await?
        .into_iter()
        .map(|s| (s.id, s))
        .collect();
    let deployment_map: HashMap<Uuid, deployments::Model> = deployments::Entity::find()
        .filter(deployments::Column::Id.is_in(collect(|r| r.deployment_id)))
        .all(&state.db)
        .await?
        .into_iter()
        .map(|d| (d.id, d))
        .collect();
    let calibration_map: HashMap<Uuid, calibrations::Model> = calibrations::Entity::find()
        .filter(calibrations::Column::Id.is_in(collect(|r| r.calibration_id)))
        .all(&state.db)
        .await?
        .into_iter()
        .map(|c| (c.id, c))
        .collect();
    let curve_map: HashMap<Uuid, standard_curves::Model> = standard_curves::Entity::find()
        .filter(standard_curves::Column::Id.is_in(collect(|r| r.standard_curve_id)))
        .all(&state.db)
        .await?
        .into_iter()
        .map(|c| (c.id, c))
        .collect();
    let event_map: HashMap<Uuid, collection_events::Model> = collection_events::Entity::find()
        .filter(collection_events::Column::Id.is_in(collect(|r| r.collection_event_id)))
        .all(&state.db)
        .await?
        .into_iter()
        .map(|e| (e.id, e))
        .collect();
    let sample_map: HashMap<Uuid, samples::Model> = samples::Entity::find()
        .filter(samples::Column::Id.is_in(collect(|r| r.sample_id)))
        .all(&state.db)
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
        .all(&state.db)
        .await?
        .into_iter()
        .map(|s| (s.id, s.name))
        .collect();

    let mut records = Vec::with_capacity(groups.len());
    for (stream_id, group) in &groups {
        let stream = streams
            .get(stream_id)
            .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

        let receipt = fetch_covering_receipt(&state.db, *stream_id, q.time).await?;
        let holds = fetch_holds(&state.db, *stream_id, group[0], q.time).await?;

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
                withdrawn_reason: r.withdrawn_reason.clone(),
                ingested_at: r.ingested_at,
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
        let computation = match group
            .iter()
            .find_map(|r| r.sample_id)
            .and_then(|id| sample_map.get(&id))
        {
            Some(sample) => {
                let run_source = match sample
                    .provenance
                    .as_ref()
                    .and_then(|p| p.get("run_id"))
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
                {
                    Some(run_id) => fetch_run_source(&state.db, run_id).await?,
                    None => None,
                };
                Some(ComputationInfo {
                    sample_id: sample.id,
                    created_by: sample.created_by.clone(),
                    provenance: sample.provenance.clone(),
                    run_source,
                    sd_estimator: sample.sd_estimator.clone(),
                    sd_estimator_source: sample.sd_estimator_source.clone(),
                })
            }
            None => None,
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
                receipt,
            },
            readings: readings_out,
            chain: ChainInfo { sensor, deployment },
            event,
            computation,
            holds,
        });
    }

    Ok(Json(ProvenanceResponse {
        time: q.time,
        site_id: rows.iter().find_map(|r| r.site_id).or(q.site_id),
        parameter_id: rows.iter().find_map(|r| r.parameter_id).or(q.parameter_id),
        duplicate_slot: records.len() > 1,
        records,
    }))
}

fn sanitize_cadence(value: &str) -> AppResult<&str> {
    match value {
        "spot" | "derived" => Ok(value),
        _ => Err(AppError::BadRequest(format!(
            "measurement_type must be continuous, spot or derived, got '{value}'"
        ))),
    }
}

async fn fetch_covering_receipt(
    db: &sea_orm::DatabaseConnection,
    stream_id: Uuid,
    time: DateTime<Utc>,
) -> AppResult<Option<ReceiptSummary>> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, at, window_from, window_to, submitted, new_rows, changed, unchanged, \
                    withdrawn, rejected_total, braked \
             FROM ingest_receipts \
             WHERE stream_id = $1 AND window_from <= $2 AND window_to >= $2 \
             ORDER BY at DESC LIMIT 1",
            [stream_id.into(), time.into()],
        ))
        .await?;
    let Some(row) = row else { return Ok(None) };
    let fixed = |name: &str| -> Option<DateTime<Utc>> {
        row.try_get::<Option<DateTime<chrono::FixedOffset>>>("", name)
            .ok()
            .flatten()
            .map(|t| t.with_timezone(&Utc))
    };
    Ok(Some(ReceiptSummary {
        id: row.try_get("", "id").map_err(AppError::Database)?,
        at: fixed("at").unwrap_or(time),
        window_from: fixed("window_from"),
        window_to: fixed("window_to"),
        submitted: row.try_get("", "submitted").map_err(AppError::Database)?,
        new_rows: row.try_get("", "new_rows").map_err(AppError::Database)?,
        changed: row.try_get("", "changed").map_err(AppError::Database)?,
        unchanged: row.try_get("", "unchanged").map_err(AppError::Database)?,
        withdrawn: row.try_get("", "withdrawn").map_err(AppError::Database)?,
        rejected_total: row
            .try_get("", "rejected_total")
            .map_err(AppError::Database)?,
        braked: row.try_get("", "braked").map_err(AppError::Database)?,
    }))
}

/// Review holds touching the instant: replicate-statistics holds by stream, event-audit findings
/// and reconciliation holds by slot. Terminal holds are left out.
async fn fetch_holds(
    db: &sea_orm::DatabaseConnection,
    stream_id: Uuid,
    sample_row: &RawRow,
    time: DateTime<Utc>,
) -> AppResult<Vec<HoldRef>> {
    let mut holds = Vec::new();
    let mut push_rows = |rows: Vec<sea_orm::QueryResult>| -> AppResult<()> {
        for row in rows {
            let created: DateTime<chrono::FixedOffset> =
                row.try_get("", "created_at").map_err(AppError::Database)?;
            holds.push(HoldRef {
                id: row.try_get("", "id").map_err(AppError::Database)?,
                kind: row.try_get("", "kind").map_err(AppError::Database)?,
                status: row.try_get("", "status").map_err(AppError::Database)?,
                created_at: created.with_timezone(&Utc),
            });
        }
        Ok(())
    };

    let by_stream = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, kind, status, created_at FROM replicate_audit_holds \
             WHERE stream_id = $1 AND group_time = $2 \
               AND status IN ('pending', 'deferred', 'acknowledged') \
             ORDER BY created_at DESC",
            [stream_id.into(), time.into()],
        ))
        .await?;
    push_rows(by_stream)?;

    if let (Some(site_id), Some(parameter_id)) = (sample_row.site_id, sample_row.parameter_id) {
        let by_slot = db
            .query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT id, kind, status, created_at FROM replicate_audit_holds \
                 WHERE stream_id IS NULL AND site_id = $1 AND parameter_id = $2 \
                   AND group_time = $3 AND status IN ('pending', 'deferred', 'acknowledged') \
                 ORDER BY created_at DESC",
                [site_id.into(), parameter_id.into(), time.into()],
            ))
            .await?;
        push_rows(by_slot)?;
    }
    Ok(holds)
}

async fn fetch_run_source(
    db: &sea_orm::DatabaseConnection,
    run_id: Uuid,
) -> AppResult<Option<String>> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT source FROM tool_runs WHERE id = $1",
            [run_id.into()],
        ))
        .await?;
    Ok(row.and_then(|r| r.try_get("", "source").ok()))
}
