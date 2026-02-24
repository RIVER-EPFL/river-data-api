use axum::{
    extract::{Path, Query, State},
    http::header::HeaderMap,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, QueryOrder, Statement};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::{parameters, projects};
use crate::error::{AppError, AppResult};
use crate::routes::{cache, resolve_site};
use crate::services::bulk::{self, StreamableParam};

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

impl StreamableParam for ParameterData {
    fn name(&self) -> &str {
        &self.name
    }
    fn value_at(&self, index: usize) -> Option<f64> {
        self.values.get(index).and_then(|v| *v)
    }
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
    path = "/api/private/sites/{site_id}/readings",
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
    if let (Some(start), Some(end)) = (query.start, query.end)
        && end <= start {
            return Err(AppError::BadRequest(
                "end time must be after start time".to_string(),
            ));
        }

    // Determine format from query or Accept header
    let format = bulk::determine_format(&query.format, &headers);

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
    if format == "json"
        && let Some(cached) = cache::get_cached(&state, &cache_key, &param_ids, query.end).await {
            return cache::json_response((*cached).clone(), true);
        }

    let _permit = bulk::acquire_bulk_permit(&format)?;

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
        r"r.parameter_id, r.time, COALESCE(r.calibrated_value, r.raw_value) AS value,
            CASE
                WHEN t.parameter_id IS NULL THEN NULL
                WHEN (t.alarm_min IS NOT NULL AND COALESCE(r.calibrated_value, r.raw_value) < t.alarm_min) OR
                     (t.alarm_max IS NOT NULL AND COALESCE(r.calibrated_value, r.raw_value) > t.alarm_max) THEN 2
                WHEN (t.warning_min IS NOT NULL AND COALESCE(r.calibrated_value, r.raw_value) < t.warning_min) OR
                     (t.warning_max IS NOT NULL AND COALESCE(r.calibrated_value, r.raw_value) > t.warning_max) THEN 1
                ELSE 0
            END::smallint as severity"
    } else {
        "r.parameter_id, r.time, COALESCE(r.calibrated_value, r.raw_value) AS value"
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
        "csv" => bulk::build_csv_response(&times, &param_data),
        "ndjson" => bulk::build_ndjson_response(&times, &param_data),
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
