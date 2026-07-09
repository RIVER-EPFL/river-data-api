use axum::{
    Json,
    extract::{Path, Query, State},
    http::header::HeaderMap,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, QueryOrder, Statement,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::routes::private::{parameters, site_parameters};
use crate::error::{AppError, AppResult};
use crate::routes::{cache, validate_optional_time_range, resolve_site_with_project};
use crate::common::bulk::{self, StreamableParam};

use super::types::{ProjectRef, SiteRef};

/// Per-parameter reading row: (time, value, is_flagged, flag_reason, measurement_type).
type ReadingRowTuple = Vec<(DateTime<Utc>, f64, Option<bool>, Option<String>, Option<String>)>;
/// Per-parameter severity: (time, severity).
type SeverityVec = Vec<(DateTime<Utc>, Option<i16>)>;
/// Per-parameter flag info: (time, is_flagged, flag_reason).
type FlagVec = Vec<(DateTime<Utc>, Option<bool>, Option<String>)>;

/// Minimal struct for efficient readings query
#[derive(Debug, FromQueryResult)]
struct ReadingRow {
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::FixedOffset>,
    value: f64,
    is_flagged: Option<bool>,
    flag_reason: Option<String>,
    measurement_type: Option<String>,
}

#[derive(Debug, FromQueryResult)]
struct ReadingRowWithSeverity {
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::FixedOffset>,
    value: f64,
    severity: Option<i16>,
    is_flagged: Option<bool>,
    flag_reason: Option<String>,
    measurement_type: Option<String>,
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
    /// Global parameter id (the catalog parameter this site_parameter references)
    pub parameter_id: Uuid,
    /// Stable parameter code (catalog `code`) — used as the CSV/NDJSON column key
    pub code: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(rename = "type")]
    pub sensor_type: String,
    pub units: Option<String>,
    /// Values array (same length as times, null for missing data)
    pub values: Vec<Option<f64>>,
    /// Severity levels (0=ok, 1=warning, 2=alarm). Only present when alarms=true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severities: Option<Vec<Option<i16>>>,
    /// Boolean flags marking outliers (same length as times). Only present when `include_flagged=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flagged: Option<Vec<Option<bool>>>,
    /// Reasons for flagging (same length as times). Only present when `include_flagged=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flag_reasons: Option<Vec<Option<String>>>,
    /// Per-point measurement type (continuous/spot/derived). Only present when `include_measurement_type=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measurement_types: Option<Vec<Option<String>>>,
}

impl StreamableParam for ParameterData {
    fn column_key(&self) -> &str {
        &self.code
    }
    fn parameter_id(&self) -> Option<Uuid> {
        Some(self.parameter_id)
    }
    fn value_at(&self, index: usize) -> Option<f64> {
        self.values.get(index).and_then(|v| *v)
    }
    fn measurement_type_at(&self, index: usize) -> Option<&str> {
        self.measurement_types
            .as_ref()
            .and_then(|v| v.get(index))
            .and_then(|o| o.as_deref())
    }
    fn has_measurement_types(&self) -> bool {
        self.measurement_types.is_some()
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
    /// Filter to a specific subset of parameters (comma-separated UUIDs). If omitted, returns all parameters configured for the site.
    pub parameter_ids: Option<String>,
    /// Response format: json (default), ndjson, csv
    #[serde(default = "default_format")]
    pub format: String,
    /// Include alarm severity data (threshold violations)
    pub alarms: Option<bool>,
    /// Filter by measurement type: continuous (default), spot, derived
    pub measurement_type: Option<String>,
    /// Include flagged readings with flag metadata (default: true). When false, excludes flagged readings entirely.
    pub include_flagged: Option<bool>,
    /// Include replicate readings (default: false). When false, only returns replicate_index = 0.
    pub include_replicates: Option<bool>,
    /// Filter by sample ID to retrieve replicates for a specific sample.
    pub sample_id: Option<Uuid>,
    /// Include a per-point measurement_type indicator (continuous/spot/derived) on each parameter.
    pub include_measurement_type: Option<bool>,
}

/// Get readings for a specific site
///
/// Returns time-series data for all parameters in the specified site.
/// Supports JSON, CSV, and NDJSON formats.
#[utoipa::path(
    get,
    path = "/{site_id}/readings",
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

    let effective_start = query.start.unwrap_or_else(|| {
        chrono::Utc::now() - chrono::Duration::days(state.config.default_readings_lookback_days)
    });
    let effective_end = query.end;
    validate_optional_time_range(Some(effective_start), effective_end)?;

    // Determine format from query or Accept header
    let format = bulk::determine_format(&query.format, &headers);

    // Build site_parameter query for this site only
    let mut param_query = site_parameters::Entity::find()
        .filter(site_parameters::Column::IsActive.eq(true))
        .filter(site_parameters::Column::SiteId.eq(site.id));

    if let Some(ref types) = query.sensor_types {
        let type_list: Vec<String> = types.split(',').map(|s| s.trim().to_string()).collect();
        if !type_list.is_empty() {
            param_query = param_query.filter(site_parameters::Column::SensorType.is_in(type_list));
        }
    }

    if let Some(ref ids) = query.parameter_ids {
        let parsed: Vec<Uuid> = ids
            .split(',')
            .filter_map(|s| Uuid::parse_str(s.trim()).ok())
            .collect();
        if parsed.is_empty() {
            return Err(AppError::BadRequest(
                "parameter_ids was provided but no UUIDs could be parsed".to_string(),
            ));
        }
        param_query = param_query.filter(site_parameters::Column::ParameterId.is_in(parsed));
    }

    // Get matching site_parameters (needed for cache key validation)
    let params_list = param_query
        .order_by_asc(site_parameters::Column::Name)
        .all(&state.db)
        .await?;

    // Global parameter IDs from site_parameters (readings table uses global parameter_id)
    let param_ids: Vec<Uuid> = params_list.iter().map(|p| p.parameter_id).collect();

    // Map from global parameter_id -> site_parameter info for building response
    let _param_info_map: HashMap<Uuid, &site_parameters::Model> =
        params_list.iter().map(|p| (p.parameter_id, p)).collect();

    // Fetch global parameter friendly names + stable codes
    let global_params = parameters::Entity::find()
        .filter(parameters::Column::Id.is_in(param_ids.clone()))
        .all(&state.db)
        .await?;
    let mut name_map: HashMap<Uuid, String> = HashMap::new();
    let mut code_map: HashMap<Uuid, String> = HashMap::new();
    for p in global_params {
        code_map.insert(p.id, p.code);
        if !p.name.is_empty() {
            name_map.insert(p.id, p.name);
        }
    }

    let include_alarms = query.alarms.unwrap_or(false);
    let include_flagged = query.include_flagged.unwrap_or(true);
    let include_replicates = query.include_replicates.unwrap_or(false) || query.sample_id.is_some();
    let include_measurement_type = query.include_measurement_type.unwrap_or(false);

    let measurement_type_filter = query.measurement_type.as_deref().unwrap_or("");

    // Build cache key from request parameters
    let cache_key = cache::cache_key(
        "readings",
        &[
            &site.id.to_string(),
            &effective_start.to_rfc3339(),
            &effective_end.map(|t| t.to_rfc3339()).unwrap_or_default(),
            query.sensor_types.as_deref().unwrap_or(""),
            &format,
            if include_alarms { "alarms" } else { "" },
            measurement_type_filter,
            if include_flagged { "flagged" } else { "no_flagged" },
            if include_replicates { "replicates" } else { "" },
            &query.sample_id.map(|id| id.to_string()).unwrap_or_default(),
            if include_measurement_type { "mtype" } else { "" },
        ],
    );

    // Check cache with freshness validation (JSON only)
    if format == "json"
        && let Some(cached) = cache::get_cached(&state, &cache_key, &param_ids, effective_end).await
    {
        return cache::json_response((*cached).clone(), true);
    }

    let _permit = bulk::acquire_bulk_permit(&format, &state.bulk_semaphore)?;

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

    // Build parameterized raw SQL query
    // $1 = site_id, $2..=$N+1 = parameter_ids
    let mut values: Vec<sea_orm::Value> = vec![site.id.into()];
    let placeholders: Vec<String> = param_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("${}", i + 2))
        .collect();
    values.extend(param_ids.iter().map(|id| (*id).into()));

    let select_clause: String = if include_alarms {
        // Severity from the one shared ladder (alarms engine). NULL when the slot has no threshold
        // at any tier (no `t` row); otherwise the ladder treats all-NULL bounds as 0 (disabled).
        let sev = crate::routes::private::alarms::thresholds::severity_case(
            "COALESCE(r.calibrated_value, r.raw_value)",
            "t.warning_min",
            "t.warning_max",
            "t.alarm_min",
            "t.alarm_max",
        );
        format!(
            "r.parameter_id, r.time, COALESCE(r.calibrated_value, r.raw_value) AS value, \
             CASE WHEN t.parameter_id IS NULL THEN NULL ELSE ({sev})::smallint END as severity, \
             r.is_flagged, r.flag_reason, r.measurement_type"
        )
    } else {
        "r.parameter_id, r.time, COALESCE(r.calibrated_value, r.raw_value) AS value, r.is_flagged, r.flag_reason, r.measurement_type".to_string()
    };

    let from_clause: String = if include_alarms {
        // Resolve the 3-tier threshold per slot via the single engine definition (site → global →
        // parameter default), scoped to this site, and LEFT JOIN it — so a parameter with only
        // defaults still gets a severity (the old direct join to alarm_thresholds did not).
        let cte = crate::routes::private::alarms::thresholds::resolve_thresholds_sql(
            Some(site.id),
            Some(param_ids.clone()),
        );
        format!(
            "readings r LEFT JOIN ({cte}) t ON t.parameter_id = r.parameter_id AND t.site_id = r.site_id"
        )
    } else {
        "readings r".to_string()
    };

    let next_param = param_ids.len() + 2;
    let time_conditions = match effective_end {
        Some(end) => {
            let cond = format!(
                " AND r.time >= ${} AND r.time <= ${}",
                next_param,
                next_param + 1
            );
            values.push(effective_start.into());
            values.push(end.into());
            cond
        }
        None => {
            let cond = format!(" AND r.time >= ${next_param}");
            values.push(effective_start.into());
            cond
        }
    };

    let measurement_type_condition = if measurement_type_filter.is_empty() {
        String::new()
    } else if measurement_type_filter == "continuous" {
        // "continuous" also covers legacy rows written before the measurement_type
        // column existed (NULL), so the field-sensor line keeps its historical data.
        " AND (r.measurement_type = 'continuous' OR r.measurement_type IS NULL)".to_string()
    } else {
        let idx = values.len() + 1;
        values.push(measurement_type_filter.to_string().into());
        format!(" AND r.measurement_type = ${idx}")
    };

    let flagged_condition = if include_flagged {
        ""
    } else {
        " AND (r.is_flagged IS NOT TRUE)"
    };

    let replicate_condition = if include_replicates {
        ""
    } else {
        " AND r.replicate_index = 0"
    };

    let sample_id_condition = if let Some(sid) = query.sample_id {
        let idx = values.len() + 1;
        values.push(sid.into());
        format!(" AND r.sample_id = ${idx}")
    } else {
        String::new()
    };

    let order_clause = if include_replicates {
        "ORDER BY r.parameter_id, r.time, r.replicate_index"
    } else {
        "ORDER BY r.parameter_id, r.time"
    };

    let sql = format!(
        "SELECT {select_clause} FROM {from_clause} WHERE r.site_id = $1 AND r.parameter_id IN ({}){time_conditions}{measurement_type_condition}{flagged_condition}{replicate_condition}{sample_id_condition} {order_clause}",
        placeholders.join(",")
    );

    let query_result = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?;

    let estimated_times = query_result.len() / num_params.max(1);

    // When include_replicates is true, multiple rows can share the same timestamp
    // (different replicate_index). We use sequential indexing to preserve all rows
    // instead of the HashSet deduplication that would collapse them.
    if include_replicates {
        let mut param_rows: HashMap<Uuid, ReadingRowTuple> =
            HashMap::with_capacity(num_params);

        for row in &query_result {
            if let Ok(r) = ReadingRow::from_query_result(row, "") {
                let time = r.time.with_timezone(&Utc);
                param_rows
                    .entry(r.parameter_id)
                    .or_insert_with(|| Vec::with_capacity(estimated_times))
                    .push((time, r.value, r.is_flagged, r.flag_reason, r.measurement_type));
            }
        }

        // Build times from the parameter with the most rows (all params should have same
        // timestamps when querying by sample_id, but be defensive)
        let max_param = param_rows.values().max_by_key(|v| v.len());
        let times: Vec<DateTime<Utc>> = max_param
            .map(|rows| rows.iter().map(|(t, ..)| *t).collect())
            .unwrap_or_default();

        let param_data: Vec<ParameterData> = params_list
            .iter()
            .map(|sp| {
                let rows = param_rows.get(&sp.parameter_id);
                let len = times.len();
                let mut values = vec![None; len];
                let mut flagged_vec: Option<Vec<Option<bool>>> = if include_flagged {
                    Some(vec![None; len])
                } else {
                    None
                };
                let mut flag_reasons_vec: Option<Vec<Option<String>>> = if include_flagged {
                    Some(vec![None; len])
                } else {
                    None
                };
                let mut measurement_types_vec: Option<Vec<Option<String>>> = if include_measurement_type {
                    Some(vec![None; len])
                } else {
                    None
                };

                if let Some(rows) = rows {
                    for (i, (_, value, is_flag, reason, mtype)) in rows.iter().enumerate() {
                        if i < len {
                            values[i] = Some(*value);
                            if let Some(ref mut fv) = flagged_vec {
                                fv[i] = *is_flag;
                            }
                            if let Some(ref mut rv) = flag_reasons_vec {
                                rv[i] = reason.clone();
                            }
                            if let Some(ref mut mtv) = measurement_types_vec {
                                mtv[i] = mtype.clone();
                            }
                        }
                    }
                }

                ParameterData {
                    id: sp.id,
                    parameter_id: sp.parameter_id,
                    code: code_map.get(&sp.parameter_id).cloned().unwrap_or_default(),
                    name: sp.name.clone(),
                    display_name: name_map.get(&sp.parameter_id).cloned(),
                    sensor_type: if sp.sensor_type.is_empty() { sp.name.clone() } else { sp.sensor_type.clone() },
                    units: sp.display_units.clone(),
                    values,
                    severities: None,
                    flagged: flagged_vec,
                    flag_reasons: flag_reasons_vec,
                    measurement_types: measurement_types_vec,
                }
            })
            .collect();

        let actual_start = times.first().copied();
        let actual_end = times.last().copied();

        return match format.as_str() {
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
        };
    }

    let mut time_set: HashSet<DateTime<Utc>> = HashSet::with_capacity(estimated_times);
    let mut param_values: HashMap<Uuid, Vec<(DateTime<Utc>, f64)>> =
        HashMap::with_capacity(num_params);
    let mut param_severities: HashMap<Uuid, SeverityVec> = HashMap::new();
    let mut param_flags: HashMap<Uuid, FlagVec> = HashMap::new();
    let mut param_meas_types: HashMap<Uuid, Vec<(DateTime<Utc>, Option<String>)>> = HashMap::new();

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
                if include_flagged {
                    param_flags
                        .entry(r.parameter_id)
                        .or_default()
                        .push((time, r.is_flagged, r.flag_reason));
                }
                if include_measurement_type {
                    param_meas_types
                        .entry(r.parameter_id)
                        .or_default()
                        .push((time, r.measurement_type));
                }
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
                if include_flagged {
                    param_flags
                        .entry(r.parameter_id)
                        .or_default()
                        .push((time, r.is_flagged, r.flag_reason));
                }
                if include_measurement_type {
                    param_meas_types
                        .entry(r.parameter_id)
                        .or_default()
                        .push((time, r.measurement_type));
                }
            }
        }
    }

    let mut times: Vec<DateTime<Utc>> = time_set.into_iter().collect();
    times.sort_unstable();

    let time_index: HashMap<DateTime<Utc>, usize> =
        times.iter().enumerate().map(|(i, t)| (*t, i)).collect();

    let param_data: Vec<ParameterData> = params_list
        .iter()
        .map(|sp| {
            let global_param_id = sp.parameter_id;
            let mut values: Vec<Option<f64>> = vec![None; times.len()];
            let mut severities_vec: Option<Vec<Option<i16>>> = if include_alarms {
                Some(vec![None; times.len()])
            } else {
                None
            };
            let mut flagged_vec: Option<Vec<Option<bool>>> = if include_flagged {
                Some(vec![None; times.len()])
            } else {
                None
            };
            let mut flag_reasons_vec: Option<Vec<Option<String>>> = if include_flagged {
                Some(vec![None; times.len()])
            } else {
                None
            };
            let mut measurement_types_vec: Option<Vec<Option<String>>> = if include_measurement_type {
                Some(vec![None; times.len()])
            } else {
                None
            };

            if let Some(readings) = param_values.get(&global_param_id) {
                for (time, value) in readings {
                    if let Some(&idx) = time_index.get(time) {
                        values[idx] = Some(*value);
                    }
                }
            }

            if let Some(ref mut sev_vec) = severities_vec
                && let Some(sevs) = param_severities.get(&global_param_id)
            {
                for (time, severity) in sevs {
                    if let Some(&idx) = time_index.get(time) {
                        sev_vec[idx] = *severity;
                    }
                }
            }

            if let Some(flags) = param_flags.get(&global_param_id) {
                for (time, is_flag, reason) in flags {
                    if let Some(&idx) = time_index.get(time) {
                        if let Some(ref mut fv) = flagged_vec {
                            fv[idx] = *is_flag;
                        }
                        if let Some(ref mut rv) = flag_reasons_vec {
                            rv[idx] = reason.clone();
                        }
                    }
                }
            }

            if let Some(ref mut mtv) = measurement_types_vec
                && let Some(mtypes) = param_meas_types.get(&global_param_id)
            {
                for (time, mtype) in mtypes {
                    if let Some(&idx) = time_index.get(time) {
                        mtv[idx] = mtype.clone();
                    }
                }
            }

            ParameterData {
                id: sp.id,
                parameter_id: sp.parameter_id,
                code: code_map.get(&sp.parameter_id).cloned().unwrap_or_default(),
                name: sp.name.clone(),
                display_name: name_map.get(&sp.parameter_id).cloned(),
                sensor_type: if sp.sensor_type.is_empty() { sp.name.clone() } else { sp.sensor_type.clone() },
                units: sp.display_units.clone(),
                values,
                severities: severities_vec,
                flagged: flagged_vec,
                flag_reasons: flag_reasons_vec,
                measurement_types: measurement_types_vec,
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
