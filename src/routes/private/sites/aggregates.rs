use axum::{
    Json,
    extract::{Path, Query, State},
    http::header::HeaderMap,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, Statement};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::routes::private::{alarm_thresholds, site_parameters};
use crate::error::{AppError, AppResult};
use crate::routes::{cache, resolve_site_with_project, validate_time_range};
use crate::common::bulk::{self, StreamableAggregateParam};

use super::types::{ProjectRef, SiteRef};

/// Per-parameter aggregate data: (avg, min, max, count) keyed by timestamp.
type ParamAggMap = HashMap<Uuid, HashMap<DateTime<Utc>, (Option<f64>, Option<f64>, Option<f64>, i64)>>;

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
    /// Count of flagged readings per bucket (always present).
    pub flagged_count: Vec<i64>,
}

impl StreamableAggregateParam for ParameterAggregateData {
    fn name(&self) -> &str {
        &self.name
    }
    fn avg_at(&self, index: usize) -> Option<f64> {
        self.avg.get(index).and_then(|v| *v)
    }
    fn min_at(&self, index: usize) -> Option<f64> {
        self.min.get(index).and_then(|v| *v)
    }
    fn max_at(&self, index: usize) -> Option<f64> {
        self.max.get(index).and_then(|v| *v)
    }
    fn count_at(&self, index: usize) -> Option<i64> {
        self.count.get(index).copied()
    }
}

#[derive(Debug, FromQueryResult)]
struct AggregateRow {
    bucket: DateTime<Utc>,
    parameter_id: Uuid,
    avg_value: Option<f64>,
    min_value: Option<f64>,
    max_value: Option<f64>,
    count: i64,
}

#[derive(Debug, FromQueryResult)]
struct FlaggedBucketRow {
    bucket: DateTime<Utc>,
    parameter_id: Uuid,
    flagged_count: i64,
}

fn build_csv_response(
    _resolution: &str,
    times: &[DateTime<Utc>],
    params: &[ParameterAggregateData],
) -> AppResult<Response> {
    bulk::build_aggregates_csv_response(times, params)
}

fn build_ndjson_response(
    times: &[DateTime<Utc>],
    params: &[ParameterAggregateData],
) -> AppResult<Response> {
    bulk::build_aggregates_ndjson_response(times, params)
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
    path = "/{site_id}/aggregates/{resolution}",
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
    ProjectScope(scope): ProjectScope,
    headers: HeaderMap,
) -> AppResult<Response> {
    let (site, project) = resolve_site_with_project(&state.db, &site_id).await?;

    // Enforce project scope
    if let Some(scope_project) = scope
        && site.project_id != Some(scope_project)
    {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }

    let project_ref = project.map(|p| ProjectRef {
        id: p.id,
        name: p.name,
    });

    let site_ref = SiteRef {
        id: site.id,
        name: site.name.clone(),
    };

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

    validate_time_range(query.start, query.end)?;

    // Enforce max aggregate time range
    let span = query.end - query.start;
    if span.num_days() > state.config.max_aggregates_time_range_days {
        return Err(AppError::BadRequest(format!(
            "Time range exceeds maximum of {} days for aggregates",
            state.config.max_aggregates_time_range_days
        )));
    }

    let format = bulk::determine_format(&query.format, &headers);

    let mut param_query = site_parameters::Entity::find()
        .filter(site_parameters::Column::IsActive.eq(true))
        .filter(site_parameters::Column::SiteId.eq(site.id));

    if let Some(ref types) = query.sensor_types {
        let type_list: Vec<String> = types.split(',').map(|s| s.trim().to_string()).collect();
        if !type_list.is_empty() {
            param_query = param_query.filter(site_parameters::Column::SensorType.is_in(type_list));
        }
    }

    let params_list = param_query.all(&state.db).await?;
    // Global parameter IDs from site_parameters (readings/aggregates use global parameter_id)
    let param_ids: Vec<Uuid> = params_list.iter().map(|p| p.parameter_id).collect();

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
        && let Some(cached) =
            cache::get_cached(&state, &cache_key, &param_ids, Some(query.end)).await
    {
        return cache::json_response((*cached).clone(), true);
    }

    let _permit = bulk::acquire_bulk_permit(&format, &state.bulk_semaphore)?;

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
    // Prefer site-specific thresholds over global ones
    let threshold_map: HashMap<Uuid, alarm_thresholds::Model> = if include_alarms {
        let all_thresholds = alarm_thresholds::Entity::find()
            .filter(alarm_thresholds::Column::ParameterId.is_in(param_ids.clone()))
            .filter(
                sea_orm::Condition::any()
                    .add(alarm_thresholds::Column::SiteId.eq(site.id))
                    .add(alarm_thresholds::Column::SiteId.is_null()),
            )
            .all(&state.db)
            .await?;
        let mut map: HashMap<Uuid, alarm_thresholds::Model> = HashMap::new();
        for t in all_thresholds {
            let existing = map.get(&t.parameter_id);
            // Insert if no existing entry, or if this one is site-specific (preferred)
            if existing.is_none() || t.site_id.is_some() {
                map.insert(t.parameter_id, t);
            }
        }
        map
    } else {
        HashMap::new()
    };

    // $1 = site_id, $2..=$N+1 = parameter_ids
    let placeholders: Vec<String> = param_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("${}", i + 2))
        .collect();
    let mut param_values: Vec<sea_orm::Value> = vec![site.id.into()];
    param_values.extend(param_ids.iter().map(|id| (*id).into()));
    let start_param = param_ids.len() + 2;
    let end_param = start_param + 1;

    let sql = format!(
        r"
        SELECT
            bucket,
            parameter_id,
            avg_value,
            min_value,
            max_value,
            count
        FROM {view_name}
        WHERE site_id = $1
          AND parameter_id IN ({})
          AND bucket >= ${}
          AND bucket <= ${}
        ORDER BY bucket ASC, parameter_id ASC
        ",
        placeholders.join(","),
        start_param,
        end_param,
    );

    let mut values: Vec<sea_orm::Value> = param_values.clone();
    values.push(query.start.into());
    values.push(query.end.into());

    let results: Vec<AggregateRow> = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?
        .into_iter()
        .filter_map(|row| AggregateRow::from_query_result(&row, "").ok())
        .collect();

    let mut time_set: BTreeMap<DateTime<Utc>, usize> = BTreeMap::new();
    let mut param_aggs: ParamAggMap = HashMap::new();

    for row in results {
        let time = row.bucket;
        time_set.entry(time).or_insert(0);
        param_aggs.entry(row.parameter_id).or_default().insert(
            time,
            (row.avg_value, row.min_value, row.max_value, row.count),
        );
    }

    let bucket_interval = match resolution.as_str() {
        "hourly" => "1 hour",
        "daily" => "1 day",
        "weekly" => "7 days",
        "monthly" => "1 month",
        _ => unreachable!(),
    };

    let flagged_sql = format!(
        r"
        SELECT
            time_bucket('{bucket_interval}'::interval, time) AS bucket,
            parameter_id,
            COUNT(*)::bigint AS flagged_count
        FROM readings
        WHERE site_id = $1
          AND parameter_id IN ({})
          AND time >= ${}
          AND time <= ${}
          AND is_flagged = TRUE
          AND replicate_index = 0
        GROUP BY bucket, parameter_id
        ",
        placeholders.join(","),
        start_param,
        end_param,
    );

    let mut flagged_values: Vec<sea_orm::Value> = param_values.clone();
    flagged_values.push(query.start.into());
    flagged_values.push(query.end.into());

    let flagged_rows: Vec<FlaggedBucketRow> = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &flagged_sql,
            flagged_values,
        ))
        .await?
        .into_iter()
        .filter_map(|row| FlaggedBucketRow::from_query_result(&row, "").ok())
        .collect();

    let mut flagged_by_param: HashMap<Uuid, HashMap<DateTime<Utc>, i64>> = HashMap::new();
    for row in flagged_rows {
        flagged_by_param
            .entry(row.parameter_id)
            .or_default()
            .insert(row.bucket, row.flagged_count);
    }

    let times: Vec<DateTime<Utc>> = time_set.keys().copied().collect();

    let param_data: Vec<ParameterAggregateData> = params_list
        .iter()
        .map(|param| {
            let global_param_id = param.parameter_id;
            let aggs_map = param_aggs.get(&global_param_id);
            let threshold = threshold_map.get(&global_param_id);

            let mut avg = Vec::with_capacity(times.len());
            let mut min = Vec::with_capacity(times.len());
            let mut max = Vec::with_capacity(times.len());
            let mut count = Vec::with_capacity(times.len());
            let mut flagged_count = Vec::with_capacity(times.len());
            let flagged_map = flagged_by_param.get(&global_param_id);
            let mut severity_vec: Option<Vec<Option<i16>>> = if include_alarms {
                Some(Vec::with_capacity(times.len()))
            } else {
                None
            };

            for t in &times {
                flagged_count.push(flagged_map.and_then(|m| m.get(t).copied()).unwrap_or(0));
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
                sensor_type: if param.sensor_type.is_empty() { param.name.clone() } else { param.sensor_type.clone() },
                units: param.display_units.clone(),
                avg,
                min,
                max,
                count,
                max_severity: severity_vec,
                flagged_count,
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
