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
use crate::entity::{alarm_thresholds, site_parameters};
use crate::error::{AppError, AppResult};
use crate::routes::{cache, resolve_site_with_project, validate_time_range};
use crate::services::bulk;

use super::types::{AlarmViolationsResponse, ParameterViolationData, SiteAlarmsQuery};
use crate::routes::sites::{ProjectRef, SiteRef};

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

    let params_with_thresholds: Vec<ParameterWithThreshold> = params_list
        .iter()
        .filter(|p| threshold_map.contains_key(&p.parameter_id))
        .map(|p| ParameterWithThreshold {
            id: p.parameter_id,
            name: p.name.clone(),
            sensor_type: p.sensor_type.clone(),
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
    let start_param = alarm_param_ids.len() + 2;
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
        JOIN alarm_thresholds t ON r.parameter_id = t.parameter_id AND (t.site_id = r.site_id OR t.site_id IS NULL)
        WHERE r.site_id = $1
          AND r.parameter_id IN ({})
          AND r.time >= ${}
          AND r.time <= ${}
          AND {}
        ORDER BY r.time, r.parameter_id
        ",
        placeholders.join(","),
        start_param,
        end_param,
        violation_condition
    );

    let mut values: Vec<sea_orm::Value> = vec![site.id.into()];
    values.extend(alarm_param_ids.iter().map(|id| (*id).into()));
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
                sensor_type: param.sensor_type.clone(),
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
