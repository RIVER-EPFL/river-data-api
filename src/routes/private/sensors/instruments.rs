//! The instruments read surface: every instrument the sync or the inventory knows, its standard
//! curves with how much data each corrected, and the streams feeding it. One call serves the
//! streams page's Instruments tab; the per-curve drill-down lists the corrected readings the way
//! `GET /sensor_calibrations/{id}/window` lists a window's.

use axum::{
    Json,
    extract::{Path, State},
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, EntityTrait, FromQueryResult, Statement};
use serde::Serialize;
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::{ProjectScope, sensor_in_scope};
use crate::error::{AppError, AppResult};
use crate::routes::private::sensors::identity::InstrumentKind;
use crate::routes::private::sensors::{self, standard_curves};

/// The rows this file's raw queries return. Derived rather than hand-decoded so a column added to
/// a query and not to its reader is a compile error rather than a field silently left behind.
#[derive(FromQueryResult)]
struct CurveUsageRow {
    id: Uuid,
    n: i64,
    first: Option<sea_orm::prelude::DateTimeWithTimeZone>,
    last: Option<sea_orm::prelude::DateTimeWithTimeZone>,
}

#[derive(FromQueryResult)]
struct StreamRefRow {
    sensor_id: Uuid,
    id: Uuid,
    source_system: String,
    source_key: String,
    measurement_type: Option<String>,
    site_name: Option<String>,
    parameter_code: Option<String>,
}

#[derive(FromQueryResult)]
struct CurvePointRow {
    time: sea_orm::prelude::DateTimeWithTimeZone,
    replicate_index: i16,
    raw_value: f64,
    calibrated_value: Option<f64>,
    is_flagged: bool,
    site_name: Option<String>,
    parameter_code: Option<String>,
}

#[derive(FromQueryResult)]
struct SensorCurveUsageRow {
    curve_id: Uuid,
    n: i64,
    first: Option<sea_orm::prelude::DateTimeWithTimeZone>,
    last: Option<sea_orm::prelude::DateTimeWithTimeZone>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CurveOverview {
    pub id: Uuid,
    pub name: Option<String>,
    pub slope: f64,
    pub intercept: f64,
    pub r_squared: Option<f64>,
    pub source_system: Option<String>,
    pub source_key: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    /// Readings this curve corrected.
    pub reading_count: i64,
    pub first_used: Option<DateTime<Utc>>,
    pub last_used: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InstrumentStreamRef {
    pub id: Uuid,
    pub source_system: String,
    pub source_key: String,
    pub measurement_type: Option<String>,
    /// The paired slot, when the stream has one.
    pub site_name: Option<String>,
    pub parameter_code: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InstrumentOverview {
    pub id: Uuid,
    pub name: Option<String>,
    pub serial_number: Option<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    /// Kept as the flag has always read: everything that is not a field device. `kind` is what
    /// says which of the three.
    pub is_lab_instrument: bool,
    /// What this row is: `device`, `lab`, `source_parameter` or `entry_channel`. Resolved from the
    /// stored column, falling back to the flag for a row predating the backfill.
    pub kind: String,
    pub source_system: Option<String>,
    pub source_key: Option<String>,
    pub curves: Vec<CurveOverview>,
    pub streams: Vec<InstrumentStreamRef>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InstrumentsOverviewResponse {
    pub instruments: Vec<InstrumentOverview>,
}

/// Every instrument that owns a standard curve or feeds a stream, with its curves' usage counts
/// and the streams naming it. `read_data`.
#[utoipa::path(
    get,
    path = "/api/instruments/overview",
    responses((status = 200, body = InstrumentsOverviewResponse)),
    tag = "sensors"
)]
pub async fn get_instruments_overview(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<InstrumentsOverviewResponse>> {
    let db = &state.db;
    if scope.sql_project_array().is_some() {
        return Err(AppError::Forbidden(
            "The instruments overview is a cross-project view; a project-scoped token cannot read it"
                .to_string(),
        ));
    }

    // Per-curve usage in one pass over the partial index (standard_curve_id IS NOT NULL is a
    // small fraction of readings).
    #[derive(Debug)]
    struct Usage {
        count: i64,
        first: Option<DateTime<Utc>>,
        last: Option<DateTime<Utc>>,
    }
    let mut usage: HashMap<Uuid, Usage> = HashMap::new();
    for row in db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT standard_curve_id AS id, COUNT(*) AS n, MIN(time) AS first, MAX(time) AS last
             FROM readings WHERE standard_curve_id IS NOT NULL GROUP BY standard_curve_id"
                .to_string(),
        ))
        .await?
    {
        let row = CurveUsageRow::from_query_result(&row, "")?;
        usage.insert(
            row.id,
            Usage {
                count: row.n,
                first: row.first.map(|t| t.with_timezone(&Utc)),
                last: row.last.map(|t| t.with_timezone(&Utc)),
            },
        );
    }

    let mut curves_by_sensor: HashMap<Uuid, Vec<CurveOverview>> = HashMap::new();
    for c in standard_curves::Entity::find().all(db).await? {
        let u = usage.get(&c.id);
        curves_by_sensor
            .entry(c.sensor_id)
            .or_default()
            .push(CurveOverview {
                id: c.id,
                name: c.name,
                slope: c.slope,
                intercept: c.intercept,
                r_squared: c.r_squared,
                source_system: c.source_system,
                source_key: c.source_key,
                created_at: Some(c.created_at.with_timezone(&Utc)),
                reading_count: u.map_or(0, |u| u.count),
                first_used: u.and_then(|u| u.first),
                last_used: u.and_then(|u| u.last),
            });
    }
    for curves in curves_by_sensor.values_mut() {
        curves.sort_by_key(|c| std::cmp::Reverse(c.created_at));
    }

    // Streams naming an instrument, with the paired slot's names resolved in the same pass.
    let mut streams_by_sensor: HashMap<Uuid, Vec<InstrumentStreamRef>> = HashMap::new();
    for row in db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT ds.sensor_id, ds.id, ds.source_system, ds.source_key, ds.measurement_type,
                    s.name AS site_name, p.code AS parameter_code
             FROM data_streams ds
             LEFT JOIN site_parameters sp ON sp.id = ds.site_parameter_id
             LEFT JOIN sites s ON s.id = sp.site_id
             LEFT JOIN parameters p ON p.id = sp.parameter_id
             WHERE ds.sensor_id IS NOT NULL
             ORDER BY ds.source_system, ds.source_key"
                .to_string(),
        ))
        .await?
    {
        let row = StreamRefRow::from_query_result(&row, "")?;
        streams_by_sensor
            .entry(row.sensor_id)
            .or_default()
            .push(InstrumentStreamRef {
                id: row.id,
                source_system: row.source_system,
                source_key: row.source_key,
                measurement_type: row.measurement_type,
                site_name: row.site_name,
                parameter_code: row.parameter_code,
            });
    }

    let relevant: Vec<sensors::Model> = sensors::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .filter(|s| curves_by_sensor.contains_key(&s.id) || streams_by_sensor.contains_key(&s.id))
        .collect();
    let mut instruments: Vec<InstrumentOverview> = relevant
        .into_iter()
        .map(|s| {
            let kind = InstrumentKind::of(Some(s.kind.as_str()), s.is_lab_instrument);
            InstrumentOverview {
                curves: curves_by_sensor.remove(&s.id).unwrap_or_default(),
                streams: streams_by_sensor.remove(&s.id).unwrap_or_default(),
                id: s.id,
                name: s.name,
                serial_number: s.serial_number,
                manufacturer: s.manufacturer,
                model: s.model,
                is_lab_instrument: kind.is_lab_instrument(),
                kind: kind.as_str().to_string(),
                source_system: s.source_system,
                source_key: s.source_key,
            }
        })
        .collect();
    // Lab instruments first: they own the curves this tab exists for. `kind`, not the flag, which
    // reads every bookkeeping row as lab too and puts them in with the spectrophotometers.
    instruments.sort_by(|a, b| {
        (b.kind == "lab")
            .cmp(&(a.kind == "lab"))
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(Json(InstrumentsOverviewResponse { instruments }))
}

/// One reading a standard curve corrected.
#[derive(Debug, Serialize, ToSchema)]
pub struct CurveUsagePoint {
    pub time: DateTime<Utc>,
    pub replicate_index: i16,
    pub raw_value: f64,
    pub calibrated_value: Option<f64>,
    pub is_flagged: bool,
    pub site_name: Option<String>,
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

const MAX_POINTS: i64 = 2000;

/// `GET /standard_curves/{id}/usage`: the readings a curve corrected. `read_data`.
#[utoipa::path(
    get,
    path = "/api/standard_curves/{id}/usage",
    params(("id" = Uuid, Path, description = "Standard curve UUID")),
    responses(
        (status = 200, body = CurveUsageResponse),
        (status = 404, description = "Curve not found"),
    ),
    tag = "sensors"
)]
pub async fn get_curve_usage(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(curve_id): Path<Uuid>,
) -> AppResult<Json<CurveUsageResponse>> {
    let db = &state.db;
    let curve = standard_curves::Entity::find_by_id(curve_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Standard curve not found".to_string()))?;
    if !sensor_in_scope(db, &scope, curve.sensor_id).await? {
        return Err(AppError::NotFound("Standard curve not found".to_string()));
    }

    let count = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COUNT(*) AS n FROM readings WHERE standard_curve_id = $1",
            [curve_id.into()],
        ))
        .await?
        .map_or(0, |r| r.try_get::<i64>("", "n").unwrap_or(0));

    let points = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT r.time, r.replicate_index, r.raw_value, r.calibrated_value,
                        COALESCE(r.is_flagged, false) AS is_flagged,
                        s.name AS site_name, p.code AS parameter_code
                 FROM readings r
                 LEFT JOIN sites s ON s.id = r.site_id
                 LEFT JOIN parameters p ON p.id = r.parameter_id
                 WHERE r.standard_curve_id = $1
                 ORDER BY r.time DESC, r.replicate_index ASC
                 LIMIT {MAX_POINTS}"
            ),
            [curve_id.into()],
        ))
        .await?
        .into_iter()
        .map(|row| {
            let row = CurvePointRow::from_query_result(&row, "")?;
            Ok(CurveUsagePoint {
                time: row.time.with_timezone(&Utc),
                replicate_index: row.replicate_index,
                raw_value: row.raw_value,
                calibrated_value: row.calibrated_value,
                is_flagged: row.is_flagged,
                site_name: row.site_name,
                parameter_code: row.parameter_code,
            })
        })
        .collect::<AppResult<Vec<_>>>()?;

    Ok(Json(CurveUsageResponse {
        curve_id,
        sensor_id: curve.sensor_id,
        slope: curve.slope,
        intercept: curve.intercept,
        reading_count: count,
        points,
    }))
}

/// One curve's usage on the instrument that owns it.
#[derive(Debug, Serialize, ToSchema)]
pub struct SensorCurveUsage {
    pub curve_id: Uuid,
    /// Readings this curve corrected.
    pub reading_count: i64,
    pub first_used: Option<DateTime<Utc>>,
    pub last_used: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SensorCurveUsageResponse {
    pub sensor_id: Uuid,
    pub usage: Vec<SensorCurveUsage>,
}

/// `GET /sensors/{id}/curve_usage`: the usage figures the overview reports, for one instrument.
/// A curve nothing was corrected with is reported at zero rather than omitted. `read_data`.
#[utoipa::path(
    get,
    path = "/api/sensors/{id}/curve_usage",
    params(("id" = Uuid, Path, description = "Sensor UUID")),
    responses(
        (status = 200, body = SensorCurveUsageResponse),
        (status = 404, description = "Sensor not found"),
    ),
    tag = "sensors"
)]
pub async fn get_sensor_curve_usage(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(sensor_id): Path<Uuid>,
) -> AppResult<Json<SensorCurveUsageResponse>> {
    let db = &state.db;
    if sensors::Entity::find_by_id(sensor_id)
        .one(db)
        .await?
        .is_none()
        || !sensor_in_scope(db, &scope, sensor_id).await?
    {
        return Err(AppError::NotFound("Sensor not found".to_string()));
    }

    let usage = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT c.id AS curve_id, COUNT(r.standard_curve_id) AS n,
                    MIN(r.time) AS first, MAX(r.time) AS last
             FROM standard_curves c
             LEFT JOIN readings r ON r.standard_curve_id = c.id
             WHERE c.sensor_id = $1
             GROUP BY c.id",
            [sensor_id.into()],
        ))
        .await?
        .into_iter()
        .map(|row| {
            let row = SensorCurveUsageRow::from_query_result(&row, "")?;
            Ok(SensorCurveUsage {
                curve_id: row.curve_id,
                reading_count: row.n,
                first_used: row.first.map(|t| t.with_timezone(&Utc)),
                last_used: row.last.map(|t| t.with_timezone(&Utc)),
            })
        })
        .collect::<AppResult<Vec<_>>>()?;

    Ok(Json(SensorCurveUsageResponse { sensor_id, usage }))
}
