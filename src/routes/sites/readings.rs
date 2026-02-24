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
use crate::entity::{parameters, projects};
use crate::error::{AppError, AppResult};
use crate::routes::{cache, resolve_site};

use super::types::{ProjectRef, SiteRef};

/// Minimal struct for efficient readings query
#[derive(Debug, FromQueryResult)]
struct ReadingRow {
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::FixedOffset>,
    value: f64,
}

#[derive(Debug, FromQueryResult)]
struct ReadingRowWithSeverity {
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::FixedOffset>,
    value: f64,
    severity: Option<i16>,
}

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
pub struct ReadingsResponse {
    /// Project this data belongs to
    pub project: Option<ProjectRef>,
    /// Site this data belongs to
    pub site: SiteRef,
    /// Start of time range (null if no data)
    pub start: Option<DateTime<Utc>>,
    /// End of time range (null if no data)
    pub end: Option<DateTime<Utc>>,
    /// Array of timestamps (aligned to 10-minute intervals)
    pub times: Vec<DateTime<Utc>>,
    /// Array of parameters with their values
    pub parameters: Vec<ParameterData>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ParameterData {
    pub id: Uuid,
    pub name: String,
    #[serde(rename = "type")]
    pub sensor_type: String,
    pub units: Option<String>,
    /// Values array (same length as times, null for missing data)
    pub values: Vec<Option<f64>>,
    /// Severity levels (0=ok, 1=warning, 2=alarm). Only present when alarms=true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severities: Option<Vec<Option<i16>>>,
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

fn build_csv_response(times: &[DateTime<Utc>], params: &[ParameterData]) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let times = times.to_vec();
    let params = params.to_vec();

    tokio::spawn(async move {
        // Header row
        let mut header = "time".to_string();
        for param in &params {
            header.push(',');
            header.push_str(&param.name);
        }
        header.push('\n');
        let _ = tx.send(Ok(header)).await;

        // Data rows
        for (i, time) in times.iter().enumerate() {
            let mut row = time.to_rfc3339();
            for param in &params {
                row.push(',');
                match param.values.get(i).and_then(|v| *v) {
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
    params: &[ParameterData],
) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let times = times.to_vec();
    let params = params.to_vec();

    tokio::spawn(async move {
        for (i, time) in times.iter().enumerate() {
            let mut obj = serde_json::Map::new();
            obj.insert("time".to_string(), serde_json::json!(time.to_rfc3339()));

            for param in &params {
                let value = param.values.get(i).and_then(|v| *v);
                obj.insert(
                    param.name.clone(),
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
pub struct SiteReadingsQuery {
    /// Start time (optional, ISO 8601). If omitted, returns from earliest data.
    pub start: Option<DateTime<Utc>>,
    /// End time (optional, ISO 8601). If omitted, returns to latest data.
    pub end: Option<DateTime<Utc>>,
    /// Filter by sensor types (comma-separated)
    pub sensor_types: Option<String>,
    /// Response format: json (default), ndjson, csv
    #[serde(default = "default_format")]
    pub format: String,
    /// Include alarm severity data (threshold violations)
    pub alarms: Option<bool>,
}

/// Get readings for a specific site
///
/// Returns time-series data for all parameters in the specified site.
/// Supports JSON, CSV, and NDJSON formats.
#[utoipa::path(
    get,
    path = "/api/sites/{site_id}/readings",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
        SiteReadingsQuery
    ),
    responses(
        (status = 200, description = "Readings retrieved successfully", body = ReadingsResponse),
        (status = 400, description = "Invalid query parameters"),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn get_site_readings(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
    Query(query): Query<SiteReadingsQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let site = resolve_site(&state.db, &site_id).await?;

    // Fetch project info if available
    let project_ref = if let Some(project_id) = site.project_id {
        projects::Entity::find_by_id(project_id)
            .one(&state.db)
            .await?
            .map(|p| ProjectRef {
                id: p.id,
                name: p.name,
            })
    } else {
        None
    };

    let site_ref = SiteRef {
        id: site.id,
        name: site.name.clone(),
    };

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

    // Build parameter query for this site only
    let mut param_query = parameters::Entity::find()
        .filter(parameters::Column::IsActive.eq(true))
        .filter(parameters::Column::SiteId.eq(site.id));

    if let Some(ref types) = query.sensor_types {
        let type_list: Vec<String> = types.split(',').map(|s| s.trim().to_string()).collect();
        if !type_list.is_empty() {
            param_query = param_query.filter(parameters::Column::SensorType.is_in(type_list));
        }
    }

    // Get matching parameters (needed for cache key validation)
    let params_list = param_query
        .order_by_asc(parameters::Column::Name)
        .all(&state.db)
        .await?;

    let param_ids: Vec<Uuid> = params_list.iter().map(|p| p.id).collect();

    let include_alarms = query.alarms.unwrap_or(false);

    // Build cache key from request parameters
    let cache_key = cache::cache_key(
        "readings",
        &[
            &site.id.to_string(),
            &query.start.map(|t| t.to_rfc3339()).unwrap_or_default(),
            &query.end.map(|t| t.to_rfc3339()).unwrap_or_default(),
            query.sensor_types.as_deref().unwrap_or(""),
            &format,
            if include_alarms { "alarms" } else { "" },
        ],
    );

    // Check cache with freshness validation (JSON only)
    if format == "json" {
        if let Some(cached) = cache::get_cached(&state, &cache_key, &param_ids, query.end).await {
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

    if params_list.is_empty() {
        return Ok(Json(ReadingsResponse {
            project: project_ref,
            site: site_ref,
            start: None,
            end: None,
            times: vec![],
            parameters: vec![],
        })
        .into_response());
    }

    let num_params = params_list.len();

    // Build optimized raw SQL query
    let param_ids_str = param_ids
        .iter()
        .map(|id| format!("'{id}'"))
        .collect::<Vec<_>>()
        .join(",");

    let select_clause = if include_alarms {
        r"r.parameter_id, r.time, r.value,
            CASE
                WHEN t.parameter_id IS NULL THEN NULL
                WHEN (t.alarm_min IS NOT NULL AND r.value < t.alarm_min) OR
                     (t.alarm_max IS NOT NULL AND r.value > t.alarm_max) THEN 2
                WHEN (t.warning_min IS NOT NULL AND r.value < t.warning_min) OR
                     (t.warning_max IS NOT NULL AND r.value > t.warning_max) THEN 1
                ELSE 0
            END::smallint as severity"
    } else {
        "r.parameter_id, r.time, r.value"
    };

    let from_clause = if include_alarms {
        "readings r LEFT JOIN alarm_thresholds t ON r.parameter_id = t.parameter_id"
    } else {
        "readings r"
    };

    let time_conditions = match (query.start, query.end) {
        (Some(start), Some(end)) => format!(
            " AND r.time >= '{}' AND r.time <= '{}'",
            start.to_rfc3339(),
            end.to_rfc3339()
        ),
        (Some(start), None) => format!(" AND r.time >= '{}'", start.to_rfc3339()),
        (None, Some(end)) => format!(" AND r.time <= '{}'", end.to_rfc3339()),
        (None, None) => String::new(),
    };

    let sql = format!(
        "SELECT {select_clause} FROM {from_clause} WHERE r.parameter_id IN ({param_ids_str}){time_conditions} ORDER BY r.parameter_id, r.time"
    );

    let query_result = state
        .db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            sql,
        ))
        .await?;

    let estimated_times = query_result.len() / num_params.max(1);
    let mut time_set: HashSet<DateTime<Utc>> = HashSet::with_capacity(estimated_times);
    let mut param_values: HashMap<Uuid, Vec<(DateTime<Utc>, f64)>> =
        HashMap::with_capacity(num_params);
    let mut param_severities: HashMap<Uuid, Vec<(DateTime<Utc>, Option<i16>)>> = HashMap::new();

    if include_alarms {
        for row in query_result {
            if let Ok(r) = ReadingRowWithSeverity::from_query_result(&row, "") {
                let time = r.time.with_timezone(&Utc);
                time_set.insert(time);
                param_values
                    .entry(r.parameter_id)
                    .or_insert_with(|| Vec::with_capacity(estimated_times))
                    .push((time, r.value));
                param_severities
                    .entry(r.parameter_id)
                    .or_default()
                    .push((time, r.severity));
            }
        }
    } else {
        for row in query_result {
            if let Ok(r) = ReadingRow::from_query_result(&row, "") {
                let time = r.time.with_timezone(&Utc);
                time_set.insert(time);
                param_values
                    .entry(r.parameter_id)
                    .or_insert_with(|| Vec::with_capacity(estimated_times))
                    .push((time, r.value));
            }
        }
    }

    let mut times: Vec<DateTime<Utc>> = time_set.into_iter().collect();
    times.sort_unstable();

    let time_index: HashMap<DateTime<Utc>, usize> = times
        .iter()
        .enumerate()
        .map(|(i, t)| (*t, i))
        .collect();

    let param_data: Vec<ParameterData> = params_list
        .iter()
        .map(|param| {
            let mut values: Vec<Option<f64>> = vec![None; times.len()];
            let mut severities_vec: Option<Vec<Option<i16>>> = if include_alarms {
                Some(vec![None; times.len()])
            } else {
                None
            };

            if let Some(readings) = param_values.get(&param.id) {
                for (time, value) in readings {
                    if let Some(&idx) = time_index.get(time) {
                        values[idx] = Some(*value);
                    }
                }
            }

            if let Some(ref mut sev_vec) = severities_vec
                && let Some(sevs) = param_severities.get(&param.id)
            {
                for (time, severity) in sevs {
                    if let Some(&idx) = time_index.get(time) {
                        sev_vec[idx] = *severity;
                    }
                }
            }

            ParameterData {
                id: param.id,
                name: param.name.clone(),
                sensor_type: param.sensor_type.clone(),
                units: param.display_units.clone(),
                values,
                severities: severities_vec,
            }
        })
        .collect();

    let actual_start = times.first().copied();
    let actual_end = times.last().copied();

    match format.as_str() {
        "csv" => build_csv_response(&times, &param_data),
        "ndjson" => build_ndjson_response(&times, &param_data),
        _ => {
            let response = ReadingsResponse {
                project: project_ref,
                site: site_ref,
                start: actual_start,
                end: actual_end,
                times,
                parameters: param_data,
            };
            cache::cache_and_respond(&state, cache_key, &response, actual_end).await
        }
    }
}
