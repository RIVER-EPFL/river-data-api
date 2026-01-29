use axum::{
    extract::{Path, Query, State},
    http::{
        header::{self, HeaderMap, HeaderValue},
        StatusCode,
    },
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, QueryOrder, Statement};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio_stream::wrappers::ReceiverStream;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::sensors;
use crate::error::{AppError, AppResult};
use crate::routes::resolve_station;

/// Minimal struct for efficient readings query
#[derive(Debug, FromQueryResult)]
struct ReadingRow {
    sensor_id: Uuid,
    time: chrono::DateTime<chrono::FixedOffset>,
    value: f64,
}

/// Global semaphore limiting concurrent bulk (CSV/NDJSON) requests.
/// Protects the database from distributed DDoS attacks.
/// Configurable via BULK_CONCURRENT_LIMIT env var (default: 5).
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
pub struct ReadingsResponse {
    /// Start of time range (null if no data)
    pub start: Option<DateTime<Utc>>,
    /// End of time range (null if no data)
    pub end: Option<DateTime<Utc>>,
    /// Array of timestamps (aligned to 10-minute intervals)
    pub times: Vec<DateTime<Utc>>,
    /// Array of sensors with their values
    pub sensors: Vec<SensorData>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SensorData {
    pub id: Uuid,
    pub name: String,
    #[serde(rename = "type")]
    pub sensor_type: String,
    pub units: Option<String>,
    pub station_id: Uuid,
    pub station: String,
    /// Values array (same length as times, null for missing data)
    pub values: Vec<Option<f64>>,
}

fn determine_format(query_format: &str, headers: &HeaderMap) -> String {
    // Query parameter takes precedence
    if query_format != "json" {
        return query_format.to_lowercase();
    }

    // Check Accept header
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

fn build_csv_response(times: &[DateTime<Utc>], sensors: &[SensorData]) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let times = times.to_vec();
    let sensors = sensors.to_vec();

    tokio::spawn(async move {
        // Header row
        let mut header = "time".to_string();
        for sensor in &sensors {
            header.push(',');
            header.push_str(&sensor.name);
        }
        header.push('\n');
        let _ = tx.send(Ok(header)).await;

        // Data rows
        for (i, time) in times.iter().enumerate() {
            let mut row = time.to_rfc3339();
            for sensor in &sensors {
                row.push(',');
                match sensor.values.get(i).and_then(|v| *v) {
                    Some(v) => row.push_str(&v.to_string()),
                    None => {} // Empty for null
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
    sensors: &[SensorData],
) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let times = times.to_vec();
    let sensors = sensors.to_vec();

    tokio::spawn(async move {
        // Each row is a JSON object with time and sensor values
        for (i, time) in times.iter().enumerate() {
            let mut obj = serde_json::Map::new();
            obj.insert("time".to_string(), serde_json::json!(time.to_rfc3339()));

            for sensor in &sensors {
                let value = sensor.values.get(i).and_then(|v| *v);
                obj.insert(
                    sensor.name.clone(),
                    match value {
                        Some(v) => serde_json::json!(v),
                        None => serde_json::Value::Null,
                    },
                );
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
pub struct StationReadingsQuery {
    /// Start time (optional, ISO 8601). If omitted, returns from earliest data.
    pub start: Option<DateTime<Utc>>,
    /// End time (optional, ISO 8601). If omitted, returns to latest data.
    pub end: Option<DateTime<Utc>>,
    /// Filter by sensor types (comma-separated)
    pub sensor_types: Option<String>,
    /// Response format: json (default), ndjson, csv
    #[serde(default = "default_format")]
    pub format: String,
}

/// Get readings for a specific station
///
/// Returns time-series data for all sensors in the specified station.
/// Supports JSON, CSV, and NDJSON formats.
#[utoipa::path(
    get,
    path = "/api/stations/{station_id}/readings",
    params(
        ("station_id" = String, Path, description = "Station UUID or name"),
        StationReadingsQuery
    ),
    responses(
        (status = 200, description = "Readings retrieved successfully", body = ReadingsResponse),
        (status = 400, description = "Invalid query parameters"),
        (status = 404, description = "Station not found"),
    ),
    tag = "readings"
)]
pub async fn get_station_readings(
    State(state): State<AppState>,
    Path(station_id): Path<String>,
    Query(query): Query<StationReadingsQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    use super::cache;

    let station = resolve_station(&state.db, &station_id).await?;

    // Validate time range if both provided
    if let (Some(start), Some(end)) = (query.start, query.end) {
        if end <= start {
            return Err(AppError::BadRequest(
                "end time must be after start time".to_string(),
            ));
        }
    }

    // Determine format from query or Accept header
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

    // Get matching sensors (needed for cache key validation)
    let sensors_list = sensor_query
        .order_by_asc(sensors::Column::Name)
        .all(&state.db)
        .await?;

    let sensor_ids: Vec<Uuid> = sensors_list.iter().map(|s| s.id).collect();

    // Build cache key from request parameters
    let cache_key = cache::cache_key(
        "readings",
        &[
            &station.id.to_string(),
            &query.start.map(|t| t.to_rfc3339()).unwrap_or_default(),
            &query.end.map(|t| t.to_rfc3339()).unwrap_or_default(),
            query.sensor_types.as_deref().unwrap_or(""),
            &format,
        ],
    );

    // Check cache with freshness validation (JSON only)
    // Pass query.end so bounded queries skip freshness check (historical data won't change)
    if format == "json" {
        if let Some(cached) = cache::get_cached(&state, &cache_key, &sensor_ids, query.end).await {
            return cache::json_response((*cached).to_vec(), true);
        }
    }

    // For bulk formats (CSV/NDJSON), acquire semaphore to limit concurrent requests
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

    if sensors_list.is_empty() {
        return Ok(Json(ReadingsResponse {
            start: None,
            end: None,
            times: vec![],
            sensors: vec![],
        })
        .into_response());
    }

    let num_sensors = sensors_list.len();

    // Build optimized raw SQL query - only fetch needed columns
    let sensor_ids_str = sensor_ids
        .iter()
        .map(|id| format!("'{id}'"))
        .collect::<Vec<_>>()
        .join(",");

    // ORDER BY sensor_id, time matches index (sensor_id, time DESC) for efficient retrieval.
    // Data arrives grouped by sensor, sorted by time - enables streaming processing in Rust.
    let sql = match (query.start, query.end) {
        (Some(start), Some(end)) => format!(
            "SELECT sensor_id, time, value FROM readings WHERE sensor_id IN ({}) AND time >= '{}' AND time <= '{}' ORDER BY sensor_id, time",
            sensor_ids_str,
            start.to_rfc3339(),
            end.to_rfc3339()
        ),
        (Some(start), None) => format!(
            "SELECT sensor_id, time, value FROM readings WHERE sensor_id IN ({}) AND time >= '{}' ORDER BY sensor_id, time",
            sensor_ids_str,
            start.to_rfc3339()
        ),
        (None, Some(end)) => format!(
            "SELECT sensor_id, time, value FROM readings WHERE sensor_id IN ({}) AND time <= '{}' ORDER BY sensor_id, time",
            sensor_ids_str,
            end.to_rfc3339()
        ),
        (None, None) => format!(
            "SELECT sensor_id, time, value FROM readings WHERE sensor_id IN ({}) ORDER BY sensor_id, time",
            sensor_ids_str
        ),
    };

    let readings_list: Vec<ReadingRow> = state
        .db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            sql,
        ))
        .await?
        .into_iter()
        .filter_map(|row| ReadingRow::from_query_result(&row, "").ok())
        .collect();

    // Data arrives sorted by (sensor_id, time) from DB.
    // 1. Collect unique times and group values by sensor in single pass
    let estimated_times = readings_list.len() / num_sensors.max(1);
    let mut time_set: HashSet<DateTime<Utc>> = HashSet::with_capacity(estimated_times);
    let mut sensor_values: HashMap<Uuid, Vec<(DateTime<Utc>, f64)>> =
        HashMap::with_capacity(num_sensors);

    for row in readings_list {
        let time = row.time.with_timezone(&Utc);
        time_set.insert(time);
        sensor_values
            .entry(row.sensor_id)
            .or_insert_with(|| Vec::with_capacity(estimated_times))
            .push((time, row.value));
    }

    // 2. Sort times once (HashSet -> sorted Vec)
    let mut times: Vec<DateTime<Utc>> = time_set.into_iter().collect();
    times.sort_unstable();

    // 3. Build time -> index map for O(1) lookup
    let time_index: HashMap<DateTime<Utc>, usize> = times
        .iter()
        .enumerate()
        .map(|(i, t)| (*t, i))
        .collect();

    // 4. Build sensor data using index map (no nested HashMap lookups)
    let sensor_data: Vec<SensorData> = sensors_list
        .iter()
        .map(|sensor| {
            let mut values: Vec<Option<f64>> = vec![None; times.len()];

            if let Some(readings) = sensor_values.get(&sensor.id) {
                for (time, value) in readings {
                    if let Some(&idx) = time_index.get(time) {
                        values[idx] = Some(*value);
                    }
                }
            }

            SensorData {
                id: sensor.id,
                name: sensor.name.clone(),
                sensor_type: sensor.sensor_type.clone(),
                units: sensor.display_units.clone(),
                station_id: sensor.station_id,
                station: station.name.clone(),
                values,
            }
        })
        .collect();

    // Use actual data range
    let actual_start = times.first().copied();
    let actual_end = times.last().copied();

    // Return appropriate format
    match format.as_str() {
        "csv" => build_csv_response(&times, &sensor_data),
        "ndjson" => build_ndjson_response(&times, &sensor_data),
        _ => {
            let response = ReadingsResponse {
                start: actual_start,
                end: actual_end,
                times,
                sensors: sensor_data,
            };
            // Cache with max_time for freshness tracking
            cache::cache_and_respond(&state, cache_key, &response, actual_end).await
        }
    }
}
