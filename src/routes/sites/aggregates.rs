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
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter};
use sea_orm::prelude::DateTimeWithTimeZone;
use sea_orm::sea_query::{Alias, Expr, Order, SelectStatement as SeaQuery};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio_stream::wrappers::ReceiverStream;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::{alarm_thresholds, parameters, projects};
use crate::error::{AppError, AppResult};
use crate::routes::{cache, resolve_site};

use super::types::{ProjectRef, SiteRef};

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
    /// Project this data belongs to
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
    pub name: String,
    #[serde(rename = "type")]
    pub sensor_type: String,
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
    pub max_severity: Option<Vec<Option<i16>>>,
}

#[derive(Debug, FromQueryResult)]
struct AggregateRow {
    bucket: DateTimeWithTimeZone,
    parameter_id: Uuid,
    avg_value: Option<f64>,
    min_value: Option<f64>,
    max_value: Option<f64>,
    count: i64,
}

fn resolution_to_view(resolution: &str) -> Result<&'static str, AppError> {
    match resolution {
        "hourly" => Ok("readings_hourly"),
        "daily" => Ok("readings_daily"),
        "weekly" => Ok("readings_weekly"),
        "monthly" => Ok("readings_monthly"),
        _ => Err(AppError::BadRequest(format!(
            "Invalid resolution: {resolution}. Must be one of: hourly, daily, weekly, monthly"
        ))),
    }
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
    params: &[ParameterAggregateData],
) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let times = times.to_vec();
    let params = params.to_vec();

    tokio::spawn(async move {
        let mut header = "time".to_string();
        for param in &params {
            header.push_str(&format!(
                ",{}_avg,{}_min,{}_max,{}_count",
                param.name, param.name, param.name, param.name
            ));
        }
        header.push('\n');
        let _ = tx.send(Ok(header)).await;

        for (i, time) in times.iter().enumerate() {
            let mut row = time.to_rfc3339();
            for param in &params {
                row.push(',');
                if let Some(v) = param.avg.get(i).and_then(|v| *v) {
                    row.push_str(&v.to_string());
                }
                row.push(',');
                if let Some(v) = param.min.get(i).and_then(|v| *v) {
                    row.push_str(&v.to_string());
                }
                row.push(',');
                if let Some(v) = param.max.get(i).and_then(|v| *v) {
                    row.push_str(&v.to_string());
                }
                row.push(',');
                if let Some(c) = param.count.get(i) {
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
    params: &[ParameterAggregateData],
) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let times = times.to_vec();
    let params = params.to_vec();

    tokio::spawn(async move {
        for (i, time) in times.iter().enumerate() {
            let mut obj = serde_json::Map::new();
            obj.insert("time".to_string(), serde_json::json!(time.to_rfc3339()));

            for param in &params {
                let avg = param.avg.get(i).and_then(|v| *v);
                let min = param.min.get(i).and_then(|v| *v);
                let max = param.max.get(i).and_then(|v| *v);
                let count = param.count.get(i).copied().unwrap_or(0);

                obj.insert(
                    format!("{}_avg", param.name),
                    avg.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
                );
                obj.insert(
                    format!("{}_min", param.name),
                    min.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
                );
                obj.insert(
                    format!("{}_max", param.name),
                    max.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
                );
                obj.insert(format!("{}_count", param.name), serde_json::json!(count));
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
pub struct SiteAggregatesQuery {
    /// Start time (required, ISO 8601)
    pub start: DateTime<Utc>,
    /// End time (required, ISO 8601)
    pub end: DateTime<Utc>,
    /// Filter by sensor types (comma-separated)
    pub sensor_types: Option<String>,
    /// Response format: json (default), ndjson, csv
    #[serde(default = "default_format")]
    pub format: String,
    /// Include alarm severity data (threshold violations)
    pub alarms: Option<bool>,
}

/// Get aggregates for a specific site
///
/// Returns aggregated parameter data for all parameters in the specified site.
/// Supports JSON, CSV, and NDJSON formats.
#[utoipa::path(
    get,
    path = "/api/private/sites/{site_id}/aggregates/{resolution}",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
        ("resolution" = String, Path, description = "Aggregation resolution: hourly, daily, weekly, monthly"),
        SiteAggregatesQuery
    ),
    responses(
        (status = 200, description = "Aggregates retrieved successfully", body = AggregatesResponse),
        (status = 400, description = "Invalid resolution or query parameters"),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn get_site_aggregates(
    State(state): State<AppState>,
    Path((site_id, resolution)): Path<(String, String)>,
    Query(query): Query<SiteAggregatesQuery>,
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

    // Validate resolution and map to view name
    let view_name = resolution_to_view(&resolution)?;

    // Validate time range
    if query.end <= query.start {
        return Err(AppError::BadRequest(
            "end time must be after start time".to_string(),
        ));
    }

    let format = determine_format(&query.format, &headers);

    let mut param_query = parameters::Entity::find()
        .filter(parameters::Column::IsActive.eq(true))
        .filter(parameters::Column::SiteId.eq(site.id));

    if let Some(ref types) = query.sensor_types {
        let type_list: Vec<String> = types.split(',').map(|s| s.trim().to_string()).collect();
        if !type_list.is_empty() {
            param_query = param_query.filter(parameters::Column::SensorType.is_in(type_list));
        }
    }

    let params_list = param_query.all(&state.db).await?;
    let param_ids: Vec<Uuid> = params_list.iter().map(|p| p.id).collect();

    let include_alarms = query.alarms.unwrap_or(false);

    let cache_key = cache::cache_key(
        "aggregates",
        &[
            &site.id.to_string(),
            &resolution,
            &query.start.to_rfc3339(),
            &query.end.to_rfc3339(),
            query.sensor_types.as_deref().unwrap_or(""),
            &format,
            if include_alarms { "alarms" } else { "" },
        ],
    );

    if format == "json"
        && let Some(cached) = cache::get_cached(&state, &cache_key, &param_ids, Some(query.end)).await {
            return cache::json_response((*cached).clone(), true);
        }

    let _permit = if format == "csv" || format == "ndjson" {
        if let Ok(permit) = BULK_SEMAPHORE.clone().try_acquire_owned() { Some(permit) } else {
            tracing::warn!(
                format = %format,
                status = StatusCode::SERVICE_UNAVAILABLE.as_u16(),
                "bulk_request_rejected"
            );
            return Err(AppError::ServiceUnavailable(
                "Too many concurrent bulk requests. Please try again later.".to_string(),
            ));
        }
    } else {
        None
    };

    if param_ids.is_empty() {
        return Ok(Json(AggregatesResponse {
            project: project_ref,
            site: site_ref,
            resolution: resolution.clone(),
            start: query.start,
            end: query.end,
            times: vec![],
            parameters: vec![],
        })
        .into_response());
    }

    // Fetch thresholds when alarms are requested (small query, ~22 rows max)
    let threshold_map: HashMap<Uuid, alarm_thresholds::Model> = if include_alarms {
        alarm_thresholds::Entity::find()
            .filter(alarm_thresholds::Column::ParameterId.is_in(param_ids.clone()))
            .all(&state.db)
            .await?
            .into_iter()
            .map(|t| (t.parameter_id, t))
            .collect()
    } else {
        HashMap::new()
    };

    // Query continuous aggregate view directly
    let view = Alias::new(view_name);

    let (sql, values) = SeaQuery::new()
        .column(Alias::new("bucket"))
        .column(Alias::new("parameter_id"))
        .column(Alias::new("avg_value"))
        .column(Alias::new("min_value"))
        .column(Alias::new("max_value"))
        .column(Alias::new("count"))
        .from(view)
        .and_where(Expr::col(Alias::new("parameter_id")).is_in(param_ids.clone()))
        .and_where(Expr::col(Alias::new("bucket")).gte(query.start))
        .and_where(Expr::col(Alias::new("bucket")).lt(query.end))
        .order_by(Alias::new("bucket"), Order::Asc)
        .build(sea_orm::sea_query::PostgresQueryBuilder);

    let stmt = sea_orm::Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values.0,
    );

    let rows: Vec<AggregateRow> = state
        .db
        .query_all(stmt)
        .await?
        .into_iter()
        .filter_map(|row| AggregateRow::from_query_result(&row, "").ok())
        .collect();

    let mut time_set: BTreeMap<DateTime<Utc>, usize> = BTreeMap::new();
    let mut param_aggs: HashMap<Uuid, HashMap<DateTime<Utc>, (Option<f64>, Option<f64>, Option<f64>, i64)>> =
        HashMap::new();

    for row in &rows {
        let bucket = row.bucket.with_timezone(&Utc);
        time_set.entry(bucket).or_insert(0);
        param_aggs
            .entry(row.parameter_id)
            .or_default()
            .insert(bucket, (row.avg_value, row.min_value, row.max_value, row.count));
    }

    let times: Vec<DateTime<Utc>> = time_set.keys().copied().collect();

    let param_data: Vec<ParameterAggregateData> = params_list
        .iter()
        .map(|param| {
            let aggs_map = param_aggs.get(&param.id);
            let threshold = threshold_map.get(&param.id);

            let mut avg = Vec::with_capacity(times.len());
            let mut min = Vec::with_capacity(times.len());
            let mut max = Vec::with_capacity(times.len());
            let mut count = Vec::with_capacity(times.len());
            let mut severity_vec: Option<Vec<Option<i16>>> = if include_alarms {
                Some(Vec::with_capacity(times.len()))
            } else {
                None
            };

            for t in &times {
                if let Some(aggs) = aggs_map.and_then(|m| m.get(t)) {
                    avg.push(aggs.0);
                    min.push(aggs.1);
                    max.push(aggs.2);
                    count.push(aggs.3);

                    if let Some(ref mut sev_vec) = severity_vec {
                        sev_vec.push(match threshold {
                            Some(th) => {
                                let min_val = aggs.1;
                                let max_val = aggs.2;
                                if (th.alarm_min.is_some()
                                    && min_val.is_some_and(|v| v < th.alarm_min.unwrap()))
                                    || (th.alarm_max.is_some()
                                        && max_val.is_some_and(|v| v > th.alarm_max.unwrap()))
                                {
                                    Some(2i16)
                                } else if (th.warning_min.is_some()
                                    && min_val.is_some_and(|v| v < th.warning_min.unwrap()))
                                    || (th.warning_max.is_some()
                                        && max_val.is_some_and(|v| v > th.warning_max.unwrap()))
                                {
                                    Some(1i16)
                                } else {
                                    Some(0i16)
                                }
                            }
                            None => None,
                        });
                    }
                } else {
                    avg.push(None);
                    min.push(None);
                    max.push(None);
                    count.push(0);
                    if let Some(ref mut sev_vec) = severity_vec {
                        sev_vec.push(None);
                    }
                }
            }

            ParameterAggregateData {
                id: param.id,
                name: param.name.clone(),
                sensor_type: param.sensor_type.clone(),
                units: param.display_units.clone(),
                avg,
                min,
                max,
                count,
                max_severity: severity_vec,
            }
        })
        .collect();

    let max_time = times.last().copied();

    match format.as_str() {
        "csv" => build_csv_response(&resolution, &times, &param_data),
        "ndjson" => build_ndjson_response(&times, &param_data),
        _ => {
            let response = AggregatesResponse {
                project: project_ref,
                site: site_ref,
                resolution,
                start: query.start,
                end: query.end,
                times,
                parameters: param_data,
            };
            cache::cache_and_respond(&state, cache_key, &response, max_time).await
        }
    }
}
