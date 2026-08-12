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
use crate::routes::private::{parameters, sites::parameters as site_parameters};
use crate::error::{AppError, AppResult};
use crate::routes::{cache, resolve_site_with_project, validate_time_range};
use crate::common::bulk::{self, StreamableAggregateParam};

use super::types::{ProjectRef, SiteRef};

/// Per-parameter aggregate data: (avg, min, max, count) keyed by timestamp.
type ParamAggMap = HashMap<Uuid, HashMap<DateTime<Utc>, (Option<f64>, Option<f64>, Option<f64>, i64)>>;

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
    /// Global parameter id (the catalog parameter this site_parameter references)
    pub parameter_id: Uuid,
    /// Owning sensor for this series. Only present when `split_by_sensor=true` (null = the
    /// unattributed/legacy group). Absent in the default collapsed response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sensor_id: Option<Uuid>,
    /// Stable parameter code (catalog `code`), used as the CSV/NDJSON column key
    pub code: String,
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
    fn column_key(&self) -> &str {
        &self.code
    }
    fn parameter_id(&self) -> Option<Uuid> {
        Some(self.parameter_id)
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

#[derive(Debug, FromQueryResult)]
struct SensorAggregateRow {
    bucket: DateTime<Utc>,
    parameter_id: Uuid,
    sensor_id: Option<Uuid>,
    avg_value: Option<f64>,
    min_value: Option<f64>,
    max_value: Option<f64>,
    count: i64,
}

#[derive(Debug, FromQueryResult)]
struct FlaggedSensorRow {
    bucket: DateTime<Utc>,
    parameter_id: Uuid,
    sensor_id: Option<Uuid>,
    flagged_count: i64,
}

fn build_csv_response(
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
    #[serde(default = "crate::common::bulk::default_format")]
    pub format: String,
    /// Include alarm severity data (threshold violations)
    pub alarms: Option<bool>,
    /// Return one series per sensor instead of collapsing the sensor dimension. JSON only; each
    /// returned parameter entry carries its `sensor_id` (null = the unattributed group).
    pub split_by_sensor: Option<bool>,
}

/// Get aggregates for a specific site
///
/// Returns aggregated parameter data for all parameters in the specified site.
/// Supports JSON, CSV, and NDJSON formats. Aggregates cover continuous and derived
/// readings only; grab samples (measurement_type 'spot') are excluded, fetch them at
/// raw resolution via the readings endpoint.
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
    if !scope.allows_project_opt(site.project_id) {
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

    // Stable parameter codes (catalog `code`) for export column keys.
    let code_map: HashMap<Uuid, String> = parameters::Entity::find()
        .filter(parameters::Column::Id.is_in(param_ids.clone()))
        .all(&state.db)
        .await?
        .into_iter()
        .map(|p| (p.id, p.code))
        .collect();

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

    // Opt-in per-sensor split: an isolated JSON-only path that returns one series per
    // (parameter, sensor) instead of collapsing the sensor dimension. Kept separate so the default
    // (cached, CSV/NDJSON-capable) path is unchanged. CSV/NDJSON ignore the flag (sensors would
    // collide on the column key); the public API is sensor-agnostic by design and has no equivalent.
    if query.split_by_sensor.unwrap_or(false) && format == "json" {
        return aggregates_split_by_sensor(
            &state,
            site_ref,
            project_ref,
            &resolution,
            view_name,
            &param_ids,
            &code_map,
            &params_list,
            query.start,
            query.end,
        )
        .await;
    }

    // Resolve thresholds via the single engine definition (site → global → parameter default),
    // scoped to this site. Replaces the old ORM fetch that ignored the parameter-default tier.
    use crate::routes::private::alarms::thresholds as alarm_engine;
    let threshold_map: HashMap<Uuid, alarm_engine::ResolvedThreshold> = if include_alarms {
        let (sql, values) =
            alarm_engine::resolve_thresholds_query(Some(site.id), Some(param_ids.clone()))
                .build(sea_orm::sea_query::PostgresQueryBuilder);
        let mut map = HashMap::new();
        for row in state
            .db
            .query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                sql,
                values.0,
            ))
            .await?
        {
            if let Ok(tr) = alarm_engine::ThresholdRow::from_query_result(&row, "") {
                map.insert(
                    tr.parameter_id,
                    alarm_engine::ResolvedThreshold {
                        warning_min: tr.warning_min,
                        warning_max: tr.warning_max,
                        alarm_min: tr.alarm_min,
                        alarm_max: tr.alarm_max,
                    },
                );
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

    // The CAGG is grouped by (bucket, site_id, parameter_id, sensor_id) since
    // m20260603_000007, so one (site, parameter) slot yields multiple rows per bucket (one per
    // sensor, plus the NULL-sensor group). Collapse the sensor dimension here so the default site
    // read is unchanged: count = SUM(count), avg = count-weighted SUM(sum_value)/SUM(count),
    // min = MIN(min_value), max = MAX(max_value).
    let sql = format!(
        r"
        SELECT
            bucket,
            parameter_id,
            CASE WHEN SUM(count) > 0 THEN SUM(sum_value) / SUM(count) ELSE NULL END AS avg_value,
            MIN(min_value) AS min_value,
            MAX(max_value) AS max_value,
            SUM(count)::bigint AS count
        FROM {view_name}
        WHERE site_id = $1
          AND parameter_id IN ({})
          AND bucket >= ${}
          AND bucket <= ${}
        GROUP BY bucket, parameter_id
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
          AND measurement_type IS DISTINCT FROM 'spot'
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
                        sev_vec.push(
                            threshold.map(|th| alarm_engine::severity_of_range(aggs.1, aggs.2, th)),
                        );
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
                parameter_id: param.parameter_id,
                sensor_id: None,
                code: code_map.get(&param.parameter_id).cloned().unwrap_or_default(),
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
        "csv" => build_csv_response(&times, &param_data),
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

/// Per-sensor aggregate read (the `split_by_sensor=true` JSON path). Returns one
/// `ParameterAggregateData` per `(parameter, sensor)` present in the sensor-dimension CAGG, each
/// carrying its `sensor_id` (null = the unattributed group). Uncached and JSON-only by design, an
/// opt-in analytical view for overlay plots, kept isolated from the default collapsed path.
#[allow(clippy::too_many_arguments)]
async fn aggregates_split_by_sensor(
    state: &AppState,
    site_ref: SiteRef,
    project_ref: Option<ProjectRef>,
    resolution: &str,
    view_name: &str,
    param_ids: &[Uuid],
    code_map: &HashMap<Uuid, String>,
    params_list: &[site_parameters::Model],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> AppResult<Response> {
    let placeholders: Vec<String> = param_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("${}", i + 2))
        .collect();
    let start_param = param_ids.len() + 2;
    let end_param = start_param + 1;

    let mut base_values: Vec<sea_orm::Value> = vec![site_ref.id.into()];
    base_values.extend(param_ids.iter().map(|id| (*id).into()));

    let sql = format!(
        r"
        SELECT
            bucket,
            parameter_id,
            sensor_id,
            CASE WHEN SUM(count) > 0 THEN SUM(sum_value) / SUM(count) ELSE NULL END AS avg_value,
            MIN(min_value) AS min_value,
            MAX(max_value) AS max_value,
            SUM(count)::bigint AS count
        FROM {view_name}
        WHERE site_id = $1
          AND parameter_id IN ({})
          AND bucket >= ${}
          AND bucket <= ${}
        GROUP BY bucket, parameter_id, sensor_id
        ORDER BY bucket ASC, parameter_id ASC, sensor_id ASC
        ",
        placeholders.join(","),
        start_param,
        end_param,
    );
    let mut values = base_values.clone();
    values.push(start.into());
    values.push(end.into());

    let rows: Vec<SensorAggregateRow> = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?
        .into_iter()
        .filter_map(|r| SensorAggregateRow::from_query_result(&r, "").ok())
        .collect();

    let bucket_interval = match resolution {
        "daily" => "1 day",
        "weekly" => "7 days",
        "monthly" => "1 month",
        _ => "1 hour",
    };
    let flagged_sql = format!(
        r"
        SELECT
            time_bucket('{bucket_interval}'::interval, time) AS bucket,
            parameter_id,
            sensor_id,
            COUNT(*)::bigint AS flagged_count
        FROM readings
        WHERE site_id = $1
          AND parameter_id IN ({})
          AND time >= ${}
          AND time <= ${}
          AND is_flagged = TRUE
          AND replicate_index = 0
          AND measurement_type IS DISTINCT FROM 'spot'
        GROUP BY bucket, parameter_id, sensor_id
        ",
        placeholders.join(","),
        start_param,
        end_param,
    );
    let mut flagged_values = base_values;
    flagged_values.push(start.into());
    flagged_values.push(end.into());
    let flagged_rows: Vec<FlaggedSensorRow> = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &flagged_sql,
            flagged_values,
        ))
        .await?
        .into_iter()
        .filter_map(|r| FlaggedSensorRow::from_query_result(&r, "").ok())
        .collect();

    type AggTuple = (Option<f64>, Option<f64>, Option<f64>, i64);
    let mut series: BTreeMap<(Uuid, Option<Uuid>), HashMap<DateTime<Utc>, AggTuple>> =
        BTreeMap::new();
    let mut flagged: HashMap<(Uuid, Option<Uuid>), HashMap<DateTime<Utc>, i64>> = HashMap::new();
    let mut time_set: std::collections::BTreeSet<DateTime<Utc>> = std::collections::BTreeSet::new();

    for r in rows {
        time_set.insert(r.bucket);
        series
            .entry((r.parameter_id, r.sensor_id))
            .or_default()
            .insert(r.bucket, (r.avg_value, r.min_value, r.max_value, r.count));
    }
    for r in flagged_rows {
        flagged
            .entry((r.parameter_id, r.sensor_id))
            .or_default()
            .insert(r.bucket, r.flagged_count);
    }

    let times: Vec<DateTime<Utc>> = time_set.into_iter().collect();
    let param_by_id: HashMap<Uuid, &site_parameters::Model> =
        params_list.iter().map(|p| (p.parameter_id, p)).collect();

    let mut parameters = Vec::with_capacity(series.len());
    for ((parameter_id, sensor_id), aggs) in series {
        let p = param_by_id.get(&parameter_id);
        let flagged_map = flagged.get(&(parameter_id, sensor_id));
        let mut avg = Vec::with_capacity(times.len());
        let mut min = Vec::with_capacity(times.len());
        let mut max = Vec::with_capacity(times.len());
        let mut count = Vec::with_capacity(times.len());
        let mut flagged_count = Vec::with_capacity(times.len());
        for t in &times {
            flagged_count.push(flagged_map.and_then(|m| m.get(t).copied()).unwrap_or(0));
            if let Some(a) = aggs.get(t) {
                avg.push(a.0);
                min.push(a.1);
                max.push(a.2);
                count.push(a.3);
            } else {
                avg.push(None);
                min.push(None);
                max.push(None);
                count.push(0);
            }
        }
        parameters.push(ParameterAggregateData {
            id: p.map_or(parameter_id, |p| p.id),
            parameter_id,
            sensor_id,
            code: code_map.get(&parameter_id).cloned().unwrap_or_default(),
            name: p.map(|p| p.name.clone()).unwrap_or_default(),
            sensor_type: p
                .map(|p| {
                    if p.sensor_type.is_empty() {
                        p.name.clone()
                    } else {
                        p.sensor_type.clone()
                    }
                })
                .unwrap_or_default(),
            units: p.and_then(|p| p.display_units.clone()),
            avg,
            min,
            max,
            count,
            max_severity: None,
            flagged_count,
        });
    }

    Ok(Json(AggregatesResponse {
        project: project_ref,
        site: site_ref,
        resolution: resolution.to_string(),
        start,
        end,
        times,
        parameters,
    })
    .into_response())
}
