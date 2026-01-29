use axum::{
    extract::{Path, Query, State},
    http::{
        header::{self, HeaderMap, HeaderValue},
        StatusCode,
    },
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Duration, Utc};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, Statement};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio_stream::wrappers::ReceiverStream;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::sensors;
use crate::error::{AppError, AppResult};
use crate::routes::resolve_station;

/// Maximum time range allowed (90 days)
const MAX_TIME_RANGE_DAYS: i64 = 90;

/// Global semaphore limiting concurrent bulk (CSV/NDJSON) requests.
static BULK_SEMAPHORE: std::sync::LazyLock<Arc<Semaphore>> = std::sync::LazyLock::new(|| {
    let limit = std::env::var("BULK_CONCURRENT_LIMIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    Arc::new(Semaphore::new(limit))
});

fn default_format() -> String {
    "json".to_string()
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AggregatesResponse {
    /// Aggregation resolution
    pub resolution: String,
    /// Start of time range
    pub start: DateTime<Utc>,
    /// End of time range
    pub end: DateTime<Utc>,
    /// Array of bucket timestamps
    pub times: Vec<DateTime<Utc>>,
    /// Array of sensors with their aggregated values
    pub sensors: Vec<SensorAggregateData>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SensorAggregateData {
    pub id: Uuid,
    pub name: String,
    #[serde(rename = "type")]
    pub sensor_type: String,
    pub units: Option<String>,
    pub station_id: Uuid,
    pub station: String,
    /// Average values array (same length as times)
    pub avg: Vec<Option<f64>>,
    /// Minimum values array
    pub min: Vec<Option<f64>>,
    /// Maximum values array
    pub max: Vec<Option<f64>>,
    /// Count of readings per bucket
    pub count: Vec<i64>,
}

#[derive(Debug, FromQueryResult)]
struct AggregateRow {
    bucket: DateTime<Utc>,
    sensor_id: Uuid,
    avg_value: Option<f64>,
    min_value: Option<f64>,
    max_value: Option<f64>,
    count: i64,
}

fn determine_format(query_format: &str, headers: &HeaderMap) -> String {
    if query_format != "json" {
        return query_format.to_lowercase();
    }

    if let Some(accept) = headers.get(header::ACCEPT)
        && let Ok(accept_str) = accept.to_str()
    {
        if accept_str.contains("application/x-ndjson") {
            return "ndjson".to_string();
        }
        if accept_str.contains("text/csv") {
            return "csv".to_string();
        }
    }

    "json".to_string()
}

fn build_csv_response(
    _resolution: &str,
    times: &[DateTime<Utc>],
    sensors: &[SensorAggregateData],
) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let times = times.to_vec();
    let sensors = sensors.to_vec();

    tokio::spawn(async move {
        // Header row: time, sensor1_avg, sensor1_min, sensor1_max, sensor1_count, sensor2_avg, ...
        let mut header = "time".to_string();
        for sensor in &sensors {
            header.push_str(&format!(
                ",{}_avg,{}_min,{}_max,{}_count",
                sensor.name, sensor.name, sensor.name, sensor.name
            ));
        }
        header.push('\n');
        let _ = tx.send(Ok(header)).await;

        // Data rows
        for (i, time) in times.iter().enumerate() {
            let mut row = time.to_rfc3339();
            for sensor in &sensors {
                // avg
                row.push(',');
                if let Some(v) = sensor.avg.get(i).and_then(|v| *v) {
                    row.push_str(&v.to_string());
                }
                // min
                row.push(',');
                if let Some(v) = sensor.min.get(i).and_then(|v| *v) {
                    row.push_str(&v.to_string());
                }
                // max
                row.push(',');
                if let Some(v) = sensor.max.get(i).and_then(|v| *v) {
                    row.push_str(&v.to_string());
                }
                // count
                row.push(',');
                if let Some(c) = sensor.count.get(i) {
                    row.push_str(&c.to_string());
                }
            }
            row.push('\n');
            if tx.send(Ok(row)).await.is_err() {
                break;
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    let body = axum::body::Body::from_stream(stream);

    Response::builder()
        .header(header::CONTENT_TYPE, HeaderValue::from_static("text/csv"))
        .body(body)
        .map_err(|e| AppError::Internal(e.to_string()))
}

fn build_ndjson_response(
    times: &[DateTime<Utc>],
    sensors: &[SensorAggregateData],
) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let times = times.to_vec();
    let sensors = sensors.to_vec();

    tokio::spawn(async move {
        for (i, time) in times.iter().enumerate() {
            let mut obj = serde_json::Map::new();
            obj.insert("time".to_string(), serde_json::json!(time.to_rfc3339()));

            for sensor in &sensors {
                let avg = sensor.avg.get(i).and_then(|v| *v);
                let min = sensor.min.get(i).and_then(|v| *v);
                let max = sensor.max.get(i).and_then(|v| *v);
                let count = sensor.count.get(i).copied().unwrap_or(0);

                obj.insert(
                    format!("{}_avg", sensor.name),
                    avg.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
                );
                obj.insert(
                    format!("{}_min", sensor.name),
                    min.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
                );
                obj.insert(
                    format!("{}_max", sensor.name),
                    max.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
                );
                obj.insert(format!("{}_count", sensor.name), serde_json::json!(count));
            }

            let line = format!("{}\n", serde_json::Value::Object(obj));
            if tx.send(Ok(line)).await.is_err() {
                break;
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    let body = axum::body::Body::from_stream(stream);

    Response::builder()
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-ndjson"),
        )
        .body(body)
        .map_err(|e| AppError::Internal(e.to_string()))
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct StationAggregatesQuery {
    /// Start time (required, ISO 8601)
    pub start: DateTime<Utc>,
    /// End time (required, ISO 8601)
    pub end: DateTime<Utc>,
    /// Filter by sensor types (comma-separated)
    pub sensor_types: Option<String>,
    /// Response format: json (default), ndjson, csv
    #[serde(default = "default_format")]
    pub format: String,
}

/// Get aggregates for a specific station
///
/// Returns aggregated sensor data for all sensors in the specified station.
/// Supports JSON, CSV, and NDJSON formats.
#[utoipa::path(
    get,
    path = "/api/stations/{station_id}/aggregates/{resolution}",
    params(
        ("station_id" = String, Path, description = "Station UUID or name"),
        ("resolution" = String, Path, description = "Aggregation resolution: hourly, daily, weekly, monthly"),
        StationAggregatesQuery
    ),
    responses(
        (status = 200, description = "Aggregates retrieved successfully", body = AggregatesResponse),
        (status = 400, description = "Invalid resolution or query parameters"),
        (status = 404, description = "Station not found"),
    ),
    tag = "aggregates"
)]
pub async fn get_station_aggregates(
    State(state): State<AppState>,
    Path((station_id, resolution)): Path<(String, String)>,
    Query(query): Query<StationAggregatesQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    use super::cache;

    let station = resolve_station(&state.db, &station_id).await?;

    // Validate resolution
    let view_name = match resolution.as_str() {
        "hourly" => "readings_hourly",
        "daily" => "readings_daily",
        "weekly" => "readings_weekly",
        "monthly" => "readings_monthly",
        _ => {
            return Err(AppError::BadRequest(format!(
                "Invalid resolution: {resolution}. Must be one of: hourly, daily, weekly, monthly"
            )));
        }
    };

    // Validate time range
    if query.end <= query.start {
        return Err(AppError::BadRequest(
            "end time must be after start time".to_string(),
        ));
    }

    // Enforce max time range
    let duration = query.end - query.start;
    if duration > Duration::days(MAX_TIME_RANGE_DAYS) {
        return Err(AppError::BadRequest(format!(
            "time range exceeds maximum of {MAX_TIME_RANGE_DAYS} days"
        )));
    }

    // Determine format
    let format = determine_format(&query.format, &headers);

    // Build sensor query for this station only
    let mut sensor_query = sensors::Entity::find()
        .filter(sensors::Column::IsActive.eq(true))
        .filter(sensors::Column::StationId.eq(station.id));

    if let Some(ref types) = query.sensor_types {
        let type_list: Vec<String> = types.split(',').map(|s| s.trim().to_string()).collect();
        if !type_list.is_empty() {
            sensor_query = sensor_query.filter(sensors::Column::SensorType.is_in(type_list));
        }
    }

    // Get matching sensors (needed for cache freshness check)
    let sensors_list = sensor_query.all(&state.db).await?;
    let sensor_ids: Vec<Uuid> = sensors_list.iter().map(|s| s.id).collect();

    // Build cache key
    let cache_key = cache::cache_key(
        "aggregates",
        &[
            &station.id.to_string(),
            &resolution,
            &query.start.to_rfc3339(),
            &query.end.to_rfc3339(),
            query.sensor_types.as_deref().unwrap_or(""),
            &format,
        ],
    );

    // Check cache with freshness validation (JSON only)
    // Aggregates always have end time, so skip freshness check (historical data won't change)
    if format == "json" {
        if let Some(cached) = cache::get_cached(&state, &cache_key, &sensor_ids, Some(query.end)).await {
            return cache::json_response((*cached).to_vec(), true);
        }
    }

    // For bulk formats, acquire semaphore to limit concurrent requests
    let _permit = if format == "csv" || format == "ndjson" {
        match BULK_SEMAPHORE.clone().try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                tracing::warn!(
                    format = %format,
                    status = StatusCode::SERVICE_UNAVAILABLE.as_u16(),
                    "bulk_request_rejected"
                );
                return Err(AppError::ServiceUnavailable(
                    "Too many concurrent bulk requests. Please try again later.".to_string(),
                ));
            }
        }
    } else {
        None
    };

    if sensor_ids.is_empty() {
        return Ok(Json(AggregatesResponse {
            resolution: resolution.clone(),
            start: query.start,
            end: query.end,
            times: vec![],
            sensors: vec![],
        })
        .into_response());
    }

    // Build the sensor_ids array for SQL
    let sensor_ids_str = sensor_ids
        .iter()
        .map(|id| format!("'{id}'"))
        .collect::<Vec<_>>()
        .join(",");

    // Query the continuous aggregate view
    let sql = format!(
        r"
        SELECT
            bucket,
            sensor_id,
            avg_value,
            min_value,
            max_value,
            count
        FROM {view_name}
        WHERE sensor_id IN ({sensor_ids_str})
          AND bucket >= $1
          AND bucket <= $2
        ORDER BY bucket ASC, sensor_id ASC
        "
    );

    let results: Vec<AggregateRow> = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            vec![query.start.into(), query.end.into()],
        ))
        .await?
        .into_iter()
        .filter_map(|row| AggregateRow::from_query_result(&row, "").ok())
        .collect();

    // Build time index and sensor value maps
    let mut time_set: BTreeMap<DateTime<Utc>, usize> = BTreeMap::new();
    let mut sensor_aggs: HashMap<Uuid, HashMap<DateTime<Utc>, (Option<f64>, Option<f64>, Option<f64>, i64)>> =
        HashMap::new();

    for row in results {
        let time = row.bucket;
        time_set.entry(time).or_insert(0);
        sensor_aggs
            .entry(row.sensor_id)
            .or_default()
            .insert(time, (row.avg_value, row.min_value, row.max_value, row.count));
    }

    // Build sorted times array
    let times: Vec<DateTime<Utc>> = time_set.keys().copied().collect();

    // Build sensor aggregate data
    let sensor_data: Vec<SensorAggregateData> = sensors_list
        .iter()
        .map(|sensor| {
            let aggs_map = sensor_aggs.get(&sensor.id);

            let mut avg = Vec::with_capacity(times.len());
            let mut min = Vec::with_capacity(times.len());
            let mut max = Vec::with_capacity(times.len());
            let mut count = Vec::with_capacity(times.len());

            for t in &times {
                if let Some(aggs) = aggs_map.and_then(|m| m.get(t)) {
                    avg.push(aggs.0);
                    min.push(aggs.1);
                    max.push(aggs.2);
                    count.push(aggs.3);
                } else {
                    avg.push(None);
                    min.push(None);
                    max.push(None);
                    count.push(0);
                }
            }

            SensorAggregateData {
                id: sensor.id,
                name: sensor.name.clone(),
                sensor_type: sensor.sensor_type.clone(),
                units: sensor.display_units.clone(),
                station_id: sensor.station_id,
                station: station.name.clone(),
                avg,
                min,
                max,
                count,
            }
        })
        .collect();

    // Get max time for cache freshness tracking
    let max_time = times.last().copied();

    // Return appropriate format
    match format.as_str() {
        "csv" => build_csv_response(&resolution, &times, &sensor_data),
        "ndjson" => build_ndjson_response(&times, &sensor_data),
        _ => {
            let response = AggregatesResponse {
                resolution,
                start: query.start,
                end: query.end,
                times,
                sensors: sensor_data,
            };
            cache::cache_and_respond(&state, cache_key, &response, max_time).await
        }
    }
}
