use axum::{
    Json,
    extract::{Path, Query, State},
    http::header::{self, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, QueryOrder, Statement,
};
use std::collections::{HashMap, HashSet};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::routes::private::{alarm_thresholds, site_parameters};
use crate::error::{AppError, AppResult};
use crate::routes::{cache, resolve_site_with_project, validate_time_range};
use crate::common::bulk;

use super::types::{
    ActiveAlarm, ActiveAlarmsResponse, AlarmSeverityCounts, AlarmSiteSummary, AlarmSummaryResponse,
    AlarmThresholdInfo, AlarmViolationsResponse, ParameterViolationData, SiteAlarmsQuery,
};
use crate::routes::private::sites::types::{ProjectRef, SiteRef};

/// Row from the violations query
#[derive(Debug, FromQueryResult)]
struct ViolationRow {
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::FixedOffset>,
    value: f64,
    severity: i16,
}

/// Parameter with threshold info for building response
struct ParameterWithThreshold {
    id: Uuid,
    name: String,
    sensor_type: String,
    display_units: Option<String>,
}

fn build_csv_response(
    times: &[DateTime<Utc>],
    params: &[ParameterViolationData],
) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let times = times.to_vec();
    let params = params.to_vec();

    tokio::spawn(async move {
        let mut header = "time".to_string();
        for param in &params {
            header.push(',');
            header.push_str(&param.name);
            header.push_str("_value,");
            header.push_str(&param.name);
            header.push_str("_severity");
        }
        header.push('\n');
        let _ = tx.send(Ok(header)).await;

        for (i, time) in times.iter().enumerate() {
            let mut row = time.to_rfc3339();
            for param in &params {
                row.push(',');
                if let Some(v) = param.values.get(i) {
                    row.push_str(&v.to_string());
                }
                row.push(',');
                if let Some(s) = param.severities.get(i) {
                    row.push_str(&s.to_string());
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
    params: &[ParameterViolationData],
) -> AppResult<Response> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

    let times = times.to_vec();
    let params = params.to_vec();

    tokio::spawn(async move {
        for (i, time) in times.iter().enumerate() {
            let mut obj = serde_json::Map::new();
            obj.insert("time".to_string(), serde_json::json!(time.to_rfc3339()));

            for param in &params {
                if let (Some(v), Some(s)) = (param.values.get(i), param.severities.get(i)) {
                    obj.insert(format!("{}_value", param.name), serde_json::json!(v));
                    obj.insert(format!("{}_severity", param.name), serde_json::json!(s));
                }
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

/// Get alarm violations for a specific site
///
/// Queries readings that violate configured thresholds within a time range.
/// Returns time-series data with severity levels (1=warning, 2=alarm).
#[utoipa::path(
    get,
    path = "/{site_id}/alarms",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
        SiteAlarmsQuery
    ),
    responses(
        (status = 200, description = "Alarm violations retrieved successfully", body = AlarmViolationsResponse),
        (status = 400, description = "Invalid query parameters"),
        (status = 404, description = "Site not found"),
    ),
    tag = "alarms"
)]
pub async fn get_site_alarms(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
    Query(query): Query<SiteAlarmsQuery>,
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

    validate_time_range(query.start, query.end)?;

    let format = bulk::determine_format(&query.format, &headers);

    // Build site_parameter query for this site
    let mut param_query = site_parameters::Entity::find()
        .filter(site_parameters::Column::IsActive.eq(true))
        .filter(site_parameters::Column::SiteId.eq(site.id));

    if let Some(ref types) = query.sensor_types {
        let type_list: Vec<String> = types.split(',').map(|s| s.trim().to_string()).collect();
        if !type_list.is_empty() {
            param_query = param_query.filter(site_parameters::Column::SensorType.is_in(type_list));
        }
    }

    let params_list = param_query
        .order_by_asc(site_parameters::Column::Name)
        .all(&state.db)
        .await?;

    if params_list.is_empty() {
        return Ok(Json(AlarmViolationsResponse {
            project: project_ref,
            site: site_ref,
            start: None,
            end: None,
            times: vec![],
            parameters: vec![],
        })
        .into_response());
    }

    // Get thresholds for these parameters (using global parameter_ids)
    let param_ids: Vec<Uuid> = params_list.iter().map(|p| p.parameter_id).collect();
    let thresholds = alarm_thresholds::Entity::find()
        .filter(alarm_thresholds::Column::ParameterId.is_in(param_ids.clone()))
        .filter(
            sea_orm::Condition::any()
                .add(alarm_thresholds::Column::SiteId.eq(site.id))
                .add(alarm_thresholds::Column::SiteId.is_null()),
        )
        .all(&state.db)
        .await?;

    // Prefer site-specific thresholds over global ones
    let mut threshold_map: HashMap<Uuid, alarm_thresholds::Model> = HashMap::new();
    for t in thresholds {
        let existing = threshold_map.get(&t.parameter_id);
        if existing.is_none() || t.site_id.is_some() {
            threshold_map.insert(t.parameter_id, t);
        }
    }

    let selected_threshold_ids: Vec<Uuid> = threshold_map.values().map(|t| t.id).collect();

    let params_with_thresholds: Vec<ParameterWithThreshold> = params_list
        .iter()
        .filter(|p| threshold_map.contains_key(&p.parameter_id))
        .map(|p| ParameterWithThreshold {
            id: p.parameter_id,
            name: p.name.clone(),
            sensor_type: if p.sensor_type.is_empty() { p.name.clone() } else { p.sensor_type.clone() },
            display_units: p.display_units.clone(),
        })
        .collect();

    if params_with_thresholds.is_empty() {
        return Ok(Json(AlarmViolationsResponse {
            project: project_ref,
            site: site_ref,
            start: None,
            end: None,
            times: vec![],
            parameters: vec![],
        })
        .into_response());
    }

    let cache_key = cache::cache_key(
        "alarms",
        &[
            &site.id.to_string(),
            &query.start.to_rfc3339(),
            &query.end.to_rfc3339(),
            &query.severity.map(|s| s.to_string()).unwrap_or_default(),
            query.sensor_types.as_deref().unwrap_or(""),
            &format,
        ],
    );

    if format == "json"
        && let Some(cached) =
            cache::get_cached(&state, &cache_key, &param_ids, Some(query.end)).await
    {
        return cache::json_response((*cached).clone(), true);
    }

    let _permit = bulk::acquire_bulk_permit(&format, &state.bulk_semaphore)?;

    let alarm_param_ids: Vec<uuid::Uuid> = params_with_thresholds.iter().map(|p| p.id).collect();
    // $1 = site_id, $2..=$N+1 = parameter_ids
    let placeholders: Vec<String> = alarm_param_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("${}", i + 2))
        .collect();
    // $N+2..=$N+M+1 = threshold_ids
    let threshold_offset = alarm_param_ids.len() + 2;
    let threshold_placeholders: Vec<String> = selected_threshold_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("${}", threshold_offset + i))
        .collect();
    let start_param = threshold_offset + selected_threshold_ids.len();
    let end_param = start_param + 1;

    let min_severity = query.severity.unwrap_or(1);

    let val_expr = "COALESCE(r.calibrated_value, r.raw_value)";

    let violation_condition = if min_severity >= 2 {
        format!(
            "(
            (t.alarm_min IS NOT NULL AND {val_expr} < t.alarm_min) OR
            (t.alarm_max IS NOT NULL AND {val_expr} > t.alarm_max)
        )"
        )
    } else {
        format!(
            "(
            (t.alarm_min IS NOT NULL AND {val_expr} < t.alarm_min) OR
            (t.alarm_max IS NOT NULL AND {val_expr} > t.alarm_max) OR
            (t.warning_min IS NOT NULL AND {val_expr} < t.warning_min) OR
            (t.warning_max IS NOT NULL AND {val_expr} > t.warning_max)
        )"
        )
    };

    let sql = format!(
        r"
        SELECT
            r.parameter_id,
            r.time,
            COALESCE(r.calibrated_value, r.raw_value) AS value,
            CASE
                WHEN (t.alarm_min IS NOT NULL AND COALESCE(r.calibrated_value, r.raw_value) < t.alarm_min) OR
                     (t.alarm_max IS NOT NULL AND COALESCE(r.calibrated_value, r.raw_value) > t.alarm_max) THEN 2
                WHEN (t.warning_min IS NOT NULL AND COALESCE(r.calibrated_value, r.raw_value) < t.warning_min) OR
                     (t.warning_max IS NOT NULL AND COALESCE(r.calibrated_value, r.raw_value) > t.warning_max) THEN 1
                ELSE 0
            END::smallint as severity
        FROM readings r
        JOIN alarm_thresholds t ON r.parameter_id = t.parameter_id AND t.id IN ({})
        WHERE r.site_id = $1
          AND r.parameter_id IN ({})
          AND r.time >= ${}
          AND r.time <= ${}
          AND {}
        ORDER BY r.time, r.parameter_id
        ",
        threshold_placeholders.join(","),
        placeholders.join(","),
        start_param,
        end_param,
        violation_condition
    );

    let mut values: Vec<sea_orm::Value> = vec![site.id.into()];
    values.extend(alarm_param_ids.iter().map(|id| (*id).into()));
    values.extend(selected_threshold_ids.iter().map(|id| (*id).into()));
    values.push(query.start.into());
    values.push(query.end.into());

    let violations: Vec<ViolationRow> = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?
        .into_iter()
        .filter_map(|row| ViolationRow::from_query_result(&row, "").ok())
        .collect();

    if violations.is_empty() {
        return Ok(Json(AlarmViolationsResponse {
            project: project_ref,
            site: site_ref,
            start: None,
            end: None,
            times: vec![],
            parameters: vec![],
        })
        .into_response());
    }

    let mut time_set: HashSet<DateTime<Utc>> = HashSet::new();
    let mut param_violations: HashMap<Uuid, Vec<(DateTime<Utc>, f64, i16)>> = HashMap::new();

    for row in violations {
        let time = row.time.with_timezone(&Utc);
        time_set.insert(time);
        param_violations
            .entry(row.parameter_id)
            .or_default()
            .push((time, row.value, row.severity));
    }

    let mut times: Vec<DateTime<Utc>> = time_set.into_iter().collect();
    times.sort_unstable();

    let time_index: HashMap<DateTime<Utc>, usize> =
        times.iter().enumerate().map(|(i, t)| (*t, i)).collect();

    let param_data: Vec<ParameterViolationData> = params_with_thresholds
        .iter()
        .filter_map(|param| {
            let violations = param_violations.get(&param.id)?;

            let mut values = vec![0.0; times.len()];
            let mut severities = vec![0i16; times.len()];

            for (time, value, severity) in violations {
                if let Some(&idx) = time_index.get(time) {
                    values[idx] = *value;
                    severities[idx] = *severity;
                }
            }

            Some(ParameterViolationData {
                id: param.id,
                name: param.name.clone(),
                sensor_type: if param.sensor_type.is_empty() { param.name.clone() } else { param.sensor_type.clone() },
                units: param.display_units.clone(),
                values,
                severities,
            })
        })
        .collect();

    let actual_start = times.first().copied();
    let actual_end = times.last().copied();

    match format.as_str() {
        "csv" => build_csv_response(&times, &param_data),
        "ndjson" => build_ndjson_response(&times, &param_data),
        _ => {
            let response = AlarmViolationsResponse {
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

/// Row from the active alarms query
#[derive(Debug, FromQueryResult)]
struct ActiveAlarmRow {
    site_id: Uuid,
    site_name: String,
    parameter_id: Uuid,
    parameter_name: String,
    current_value: f64,
    time: chrono::DateTime<chrono::FixedOffset>,
    warning_min: Option<f64>,
    warning_max: Option<f64>,
    alarm_min: Option<f64>,
    alarm_max: Option<f64>,
    severity: i16,
}

/// Fetch active alarm violations across all sites
async fn fetch_active_alarm_rows(
    db: &sea_orm::DatabaseConnection,
    scope: Option<Uuid>,
) -> AppResult<Vec<ActiveAlarmRow>> {
    let project_filter = if scope.is_some() {
        "AND s.project_id = $1"
    } else {
        ""
    };

    let sql = format!(
        r"
        WITH ranked_thresholds AS (
            SELECT DISTINCT ON (t.parameter_id, sp.site_id)
                sp.site_id,
                t.parameter_id,
                sp.name AS parameter_name,
                t.warning_min,
                t.warning_max,
                t.alarm_min,
                t.alarm_max
            FROM alarm_thresholds t
            JOIN site_parameters sp
                ON sp.parameter_id = t.parameter_id
                AND sp.is_active = true
            WHERE t.site_id = sp.site_id OR t.site_id IS NULL
            ORDER BY t.parameter_id, sp.site_id, t.site_id NULLS LAST
        ),
        latest_readings AS (
            SELECT DISTINCT ON (r.site_id, r.parameter_id)
                r.site_id,
                r.parameter_id,
                r.time,
                COALESCE(r.calibrated_value, r.raw_value) AS value
            FROM readings r
            JOIN ranked_thresholds rt
                ON rt.site_id = r.site_id
                AND rt.parameter_id = r.parameter_id
            ORDER BY r.site_id, r.parameter_id, r.time DESC
        )
        SELECT
            lr.site_id,
            s.name AS site_name,
            lr.parameter_id,
            rt.parameter_name,
            lr.value AS current_value,
            lr.time,
            rt.warning_min,
            rt.warning_max,
            rt.alarm_min,
            rt.alarm_max,
            CASE
                WHEN (rt.alarm_min IS NOT NULL AND lr.value < rt.alarm_min) OR
                     (rt.alarm_max IS NOT NULL AND lr.value > rt.alarm_max) THEN 2::smallint
                WHEN (rt.warning_min IS NOT NULL AND lr.value < rt.warning_min) OR
                     (rt.warning_max IS NOT NULL AND lr.value > rt.warning_max) THEN 1::smallint
                ELSE 0::smallint
            END AS severity
        FROM latest_readings lr
        JOIN ranked_thresholds rt
            ON rt.site_id = lr.site_id
            AND rt.parameter_id = lr.parameter_id
        JOIN sites s ON s.id = lr.site_id
        WHERE (
            (rt.alarm_min IS NOT NULL AND lr.value < rt.alarm_min) OR
            (rt.alarm_max IS NOT NULL AND lr.value > rt.alarm_max) OR
            (rt.warning_min IS NOT NULL AND lr.value < rt.warning_min) OR
            (rt.warning_max IS NOT NULL AND lr.value > rt.warning_max)
        )
        {project_filter}
        ORDER BY severity DESC, s.name, rt.parameter_name
        "
    );

    let values: Vec<sea_orm::Value> = if let Some(project_id) = scope {
        vec![project_id.into()]
    } else {
        vec![]
    };

    let rows: Vec<ActiveAlarmRow> = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?
        .into_iter()
        .filter_map(|row| ActiveAlarmRow::from_query_result(&row, "").ok())
        .collect();

    Ok(rows)
}

/// Get currently active alarm violations across all sites
///
/// For each alarm threshold, checks the latest reading to see if it
/// violates warning or alarm limits. Returns all current violations.
#[utoipa::path(
    get,
    path = "/alarms/active",
    responses(
        (status = 200, description = "Active alarm violations", body = ActiveAlarmsResponse),
    ),
    tag = "alarms"
)]
pub async fn get_active_alarms(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<ActiveAlarmsResponse>> {
    let rows = fetch_active_alarm_rows(&state.db, scope).await?;

    let alarms: Vec<ActiveAlarm> = rows
        .into_iter()
        .map(|row| ActiveAlarm {
            site_id: row.site_id,
            site_name: row.site_name,
            parameter_id: row.parameter_id,
            parameter_name: row.parameter_name,
            current_value: row.current_value,
            threshold: AlarmThresholdInfo {
                warning_min: row.warning_min,
                warning_max: row.warning_max,
                alarm_min: row.alarm_min,
                alarm_max: row.alarm_max,
            },
            severity: row.severity,
            since: row.time.with_timezone(&Utc),
        })
        .collect();

    let total = alarms.len();
    Ok(Json(ActiveAlarmsResponse { alarms, total }))
}

/// Get a summary of active alarm violations
///
/// Returns counts by severity and by site.
#[utoipa::path(
    get,
    path = "/alarms/summary",
    responses(
        (status = 200, description = "Alarm summary", body = AlarmSummaryResponse),
    ),
    tag = "alarms"
)]
pub async fn get_alarm_summary(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<AlarmSummaryResponse>> {
    let rows = fetch_active_alarm_rows(&state.db, scope).await?;

    let mut warning_count = 0usize;
    let mut alarm_count = 0usize;
    let mut site_map: HashMap<Uuid, (String, usize, usize)> = HashMap::new();

    for row in &rows {
        match row.severity {
            2 => alarm_count += 1,
            1 => warning_count += 1,
            _ => {}
        }
        let entry = site_map
            .entry(row.site_id)
            .or_insert_with(|| (row.site_name.clone(), 0, 0));
        match row.severity {
            2 => entry.2 += 1,
            1 => entry.1 += 1,
            _ => {}
        }
    }

    let total = rows.len();

    let latest_by_site = fetch_latest_reading_times(&state.db, scope).await?;

    let mut covered_sites: HashSet<Uuid> = site_map.keys().copied().collect();
    let mut by_site: Vec<AlarmSiteSummary> = site_map
        .into_iter()
        .map(|(site_id, (site_name, warnings, alarms))| AlarmSiteSummary {
            site_id,
            site_name,
            warning_count: warnings,
            alarm_count: alarms,
            latest_reading_time: latest_by_site.get(&site_id).map(|(_, t)| *t),
        })
        .collect();

    for (site_id, (site_name, latest_time)) in &latest_by_site {
        if covered_sites.insert(*site_id) {
            by_site.push(AlarmSiteSummary {
                site_id: *site_id,
                site_name: site_name.clone(),
                warning_count: 0,
                alarm_count: 0,
                latest_reading_time: Some(*latest_time),
            });
        }
    }

    by_site.sort_by(|a, b| a.site_name.cmp(&b.site_name));

    Ok(Json(AlarmSummaryResponse {
        total,
        by_severity: AlarmSeverityCounts {
            warning: warning_count,
            alarm: alarm_count,
        },
        by_site,
    }))
}

/// Row from the latest reading time query
#[derive(Debug, FromQueryResult)]
struct LatestReadingTimeRow {
    site_id: Uuid,
    site_name: String,
    latest_time: chrono::DateTime<chrono::FixedOffset>,
}

/// Fetch per-site latest reading time across all paired readings
async fn fetch_latest_reading_times(
    db: &sea_orm::DatabaseConnection,
    scope: Option<Uuid>,
) -> AppResult<HashMap<Uuid, (String, DateTime<Utc>)>> {
    let project_filter = if scope.is_some() {
        "WHERE s.project_id = $1"
    } else {
        ""
    };

    let sql = format!(
        r"
        SELECT s.id AS site_id, s.name AS site_name, MAX(r.time) AS latest_time
        FROM sites s
        JOIN readings r ON r.site_id = s.id
        {project_filter}
        GROUP BY s.id, s.name
        "
    );

    let values: Vec<sea_orm::Value> = if let Some(project_id) = scope {
        vec![project_id.into()]
    } else {
        vec![]
    };

    let rows: Vec<LatestReadingTimeRow> = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?
        .into_iter()
        .filter_map(|row| LatestReadingTimeRow::from_query_result(&row, "").ok())
        .collect();

    Ok(rows
        .into_iter()
        .map(|r| (r.site_id, (r.site_name, r.latest_time.with_timezone(&Utc))))
        .collect())
}
