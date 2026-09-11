use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use axum::Json;
use axum::Router;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::Request;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::HeaderMap;
use axum::middleware;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use chrono::DateTime;
use chrono::Utc;
use sea_orm::ActiveModelTrait;
use sea_orm::ColumnTrait;
use sea_orm::ConnectionTrait;
use sea_orm::EntityTrait;
use sea_orm::FromQueryResult;
use sea_orm::QueryFilter;
use sea_orm::QueryOrder;
use sea_orm::Set;
use sea_orm::Statement;
use serde::Deserialize;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use super::models::AcknowledgedAlarmResponse;
use super::models::ActiveAlarm;
use super::models::ActiveAlarmsResponse;
use super::models::AlarmEventResponse;
use super::models::AlarmEventsQuery;
use super::models::AlarmEventsResponse;
use super::models::AlarmSeverityCounts;
use super::models::AlarmSiteSummary;
use super::models::AlarmSummaryResponse;
use super::models::AlarmViolationsResponse;
use super::models::ParameterViolationData;
use super::models::SiteAlarmsQuery;
use super::models::ThresholdWithValue;
use super::models::ThresholdsQuery;
use super::models::alarm_event;
use super::service::ActiveAlarmRow;
use super::service::AlarmEventRow;
use super::service::ParameterWithThreshold;
use super::service::ViolationRow;
use super::service::cadence_label;
use super::service::confine_alarm_event;
use super::service::fetch_active_alarm_rows;
use super::service::fetch_last_alarm_warning_times;
use super::service::fetch_latest_reading_times;
use super::service::fetch_open_events;
use super::service::violations_query;
use crate::common::AppState;
use crate::common::bulk;
use crate::common::middleware::AuthContext;
use crate::common::middleware::ProjectScope;
use crate::common::paging::Window;
use crate::common::scope::project_filter_sql;
use crate::common::scope::require_named_target;
use crate::common::scope::require_sites_in_scope;
use crate::common::series;
use crate::common::series::Cells;
use crate::common::series::Table;
use crate::error::AppError;
use crate::error::AppResult;
use crate::routes::cache;
use crate::routes::private::reprocessing_jobs::models::QueuedJobResponse;
use crate::routes::private::site_parameters;
use crate::routes::private::sites::models::ProjectRef;
use crate::routes::private::sites::models::SiteRef;
use crate::routes::resolve_site_with_project;
use crate::routes::validate_time_range;

/// The violations export, built from the same structs the JSON body serialises. A parameter that
/// did not violate at a timestamp has no value and no severity there, in every format.
fn alarms_table(times: &[DateTime<Utc>], params: &[ParameterViolationData]) -> Table {
    let mut table = Table::at(times);
    for param in params {
        table.column(
            format!("{}_value", param.name),
            Cells::Float(param.values.clone()),
        );
        table.column(
            format!("{}_severity", param.name),
            Cells::Int(param.severities.iter().map(|s| s.map(i64::from)).collect()),
        );
    }
    table
}
/// Get alarm violations for a specific site
///
/// Queries readings that violate configured thresholds within a time range.
/// Returns time-series data with severity levels (1=warning, 2=alarm).
#[utoipa::path(
    get,
    path = "/api/sites/{site_id}/alarms",
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
    if !scope.allows_project_opt(site.project_id) {
        return Err(AppError::Forbidden(
            "That site is outside your project access".to_string(),
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

    let params_list = param_query
        .order_by_asc(site_parameters::Column::Name)
        .all(&state.db)
        .await?;

    if params_list.is_empty() {
        return empty_violations(&format, project_ref, site_ref).await;
    }

    let param_ids: Vec<Uuid> = params_list.iter().map(|p| p.parameter_id).collect();

    // Units, name and sensor_type resolve through the one slot resolver, so an alarm series
    // reports the same units the readings series and the site detail report for that slot.
    let catalog = site_parameters::catalog_map(&state.db, param_ids.iter().copied()).await?;
    let params_with_thresholds: Vec<ParameterWithThreshold> = params_list
        .iter()
        .map(|p| {
            let d = site_parameters::SlotDescriptor::resolve(p, catalog.get(&p.parameter_id));
            ParameterWithThreshold {
                id: p.parameter_id,
                name: d.slot_name,
                sensor_type: d.sensor_type,
                display_units: d.units,
            }
        })
        .collect();

    // The site id leads the key so a per-site invalidation can find every entry it owns; the query
    // is flattened in whole so a field added to it enters the key by construction.
    #[derive(serde::Serialize)]
    struct AlarmsCacheKey<'a> {
        resolved_format: &'a str,
        #[serde(flatten)]
        query: &'a SiteAlarmsQuery,
    }
    let cache_key = crate::common::cache_key::key_for(
        &format!("alarms:{}", site.id),
        &AlarmsCacheKey {
            resolved_format: &format,
            query: &query,
        },
    );

    if format == "json"
        && let Some(cached) =
            cache::get_cached(&state, &cache_key, &param_ids, Some(query.end)).await
    {
        return cache::json_response((*cached).clone(), true);
    }

    let _permit = bulk::acquire_bulk_permit(&format, &state.bulk_semaphore)?;

    let alarm_param_ids: Vec<uuid::Uuid> = params_with_thresholds.iter().map(|p| p.id).collect();

    let sql = format!(
        "{}\nORDER BY sv.time, sv.parameter_id",
        violations_query(site.id, Some(alarm_param_ids), query.severity.unwrap_or(1))
            .to_string(sea_orm::sea_query::PostgresQueryBuilder)
    );

    let values: Vec<sea_orm::Value> = vec![site.id.into(), query.start.into(), query.end.into()];

    let violations: Vec<ViolationRow> = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?
        .into_iter()
        .filter_map(|row| ViolationRow::from_query_result(&row, "").ok())
        .collect();

    if violations.is_empty() {
        return empty_violations(&format, project_ref, site_ref).await;
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

            // A timestamp where this parameter did not violate carries no value and no severity,
            // rather than a zero that reads as a measurement.
            let mut values: Vec<Option<f64>> = vec![None; times.len()];
            let mut severities: Vec<Option<i16>> = vec![None; times.len()];

            for (time, value, severity) in violations {
                if let Some(&idx) = time_index.get(time) {
                    values[idx] = Some(*value);
                    severities[idx] = Some(*severity);
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

    series::respond(
        &format,
        (times, param_data),
        |(times, params)| alarms_table(times, params),
        |(times, parameters)| async move {
            let response = AlarmViolationsResponse {
                project: project_ref,
                site: site_ref,
                start: actual_start,
                end: actual_end,
                times,
                parameters,
            };
            cache::cache_and_respond(&state, cache_key, &response, actual_end).await
        },
    )
    .await
}
/// A window with no violations, in whatever format was asked for. The empty case is the common
/// one here, so it goes through the same return point as the populated case.
async fn empty_violations(
    format: &str,
    project: Option<ProjectRef>,
    site: SiteRef,
) -> AppResult<Response> {
    let empty: (Vec<DateTime<Utc>>, Vec<ParameterViolationData>) = (Vec::new(), Vec::new());
    series::respond(
        format,
        empty,
        |(times, params)| alarms_table(times, params),
        |(times, parameters)| async move {
            Ok(Json(AlarmViolationsResponse {
                project,
                site,
                start: None,
                end: None,
                times,
                parameters,
            })
            .into_response())
        },
    )
    .await
}
/// Get currently active alarm violations across all sites
///
/// For each alarm threshold, checks the latest reading to see if it violates warning or alarm
/// limits. Each current violation is annotated with its persisted `alarm_event` (id + acknowledgement
/// state) so the UI can acknowledge it; the breach set itself stays driven by the latest readings.
#[utoipa::path(
    get,
    path = "/api/alarms/active",
    responses(
        (status = 200, description = "Active alarm violations", body = ActiveAlarmsResponse),
    ),
    tag = "alarms"
)]
pub async fn get_active_alarms(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<ActiveAlarmsResponse>> {
    let mut tagged: Vec<(ActiveAlarmRow, &'static str)> = Vec::new();
    for spot in [false, true] {
        for row in fetch_active_alarm_rows(&state.db, &scope, None, spot).await? {
            tagged.push((row, cadence_label(spot)));
        }
    }
    let open = fetch_open_events(&state.db, &scope).await?;

    let alarms: Vec<ActiveAlarm> = tagged
        .into_iter()
        .map(|(row, cadence)| {
            let ev = open.get(&(row.site_id, row.parameter_id, cadence.to_string()));
            ActiveAlarm {
                threshold: row.bounds(),
                site_id: row.site_id,
                site_name: row.site_name,
                parameter_id: row.parameter_id,
                parameter_name: row.parameter_name,
                current_value: row.current_value,
                measurement_type: cadence.to_string(),
                severity: row.severity,
                since: row.time.with_timezone(&Utc),
                started_at: ev.map(|e| e.started_at.with_timezone(&Utc)),
                event_id: ev.map(|e| e.id),
                acknowledged: ev.is_some_and(|e| e.acknowledged_at.is_some()),
                acknowledged_at: ev.and_then(|e| e.acknowledged_at.map(|t| t.with_timezone(&Utc))),
                acknowledged_by: ev.and_then(|e| e.acknowledged_by.clone()),
                max_severity: ev.map(|e| e.max_severity),
            }
        })
        .collect();

    let total = alarms.len();
    Ok(Json(ActiveAlarmsResponse { alarms, total }))
}
/// Acknowledge an open alarm event
///
/// Marks the open `alarm_event` as acknowledged by the calling user/token. Acknowledging does not
/// resolve the alarm; it stays active (flagged `acknowledged: true`) until the reading returns to
/// range. Returns 404 if the event does not exist or sits outside the caller's projects, 409 if it
/// is already resolved.
#[utoipa::path(
    post,
    path = "/api/alarms/{event_id}/acknowledge",
    params(("event_id" = String, Path, description = "Alarm event id")),
    responses(
        (status = 200, description = "Alarm acknowledged", body = AcknowledgedAlarmResponse),
        (status = 404, description = "Alarm event not found"),
        (status = 409, description = "Alarm already resolved"),
    ),
    tag = "alarms"
)]
pub async fn acknowledge_alarm(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    ProjectScope(scope): ProjectScope,
    Path(event_id): Path<Uuid>,
) -> AppResult<Json<AcknowledgedAlarmResponse>> {
    confine_alarm_event(&state, &scope, event_id).await?;

    let event = alarm_event::Entity::find_by_id(event_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Alarm event {event_id} not found")))?;

    if event.resolved_at.is_some() {
        return Err(AppError::Conflict("Alarm already resolved".to_string()));
    }
    // Idempotent: an already-acknowledged open event returns its existing acknowledgement.
    if let (Some(at), Some(by)) = (event.acknowledged_at, event.acknowledged_by.clone()) {
        return Ok(Json(AcknowledgedAlarmResponse {
            event_id,
            acknowledged_at: at,
            acknowledged_by: by,
        }));
    }

    let actor = crate::common::actor::label(&auth);
    let now = Utc::now();
    let acknowledged = alarm_event::ActiveModel {
        id: Set(event_id),
        acknowledged_at: Set(Some(now)),
        acknowledged_by: Set(Some(actor.clone())),
        updated_at: Set(now),
        ..Default::default()
    }
    .update(&state.db)
    .await?;

    Ok(Json(AcknowledgedAlarmResponse {
        event_id,
        acknowledged_at: acknowledged.acknowledged_at.unwrap_or(now),
        acknowledged_by: actor,
    }))
}
/// Remove acknowledgement from an open alarm event
///
/// Clears `acknowledged_at` and `acknowledged_by`, re-raising the alarm in the UI notification
/// badge. Returns 404 if the event does not exist or sits outside the caller's projects, 409 if
/// already resolved. Idempotent, returns 204 even if already unacknowledged.
#[utoipa::path(
    delete,
    path = "/api/alarms/{event_id}/acknowledge",
    params(("event_id" = String, Path, description = "Alarm event id")),
    responses(
        (status = 204, description = "Acknowledgement removed"),
        (status = 404, description = "Alarm event not found"),
        (status = 409, description = "Alarm already resolved"),
    ),
    tag = "alarms"
)]
pub async fn unacknowledge_alarm(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(event_id): Path<Uuid>,
) -> AppResult<StatusCode> {
    confine_alarm_event(&state, &scope, event_id).await?;

    let event = alarm_event::Entity::find_by_id(event_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Alarm event {event_id} not found")))?;

    if event.resolved_at.is_some() {
        return Err(AppError::Conflict("Alarm already resolved".to_string()));
    }

    alarm_event::ActiveModel {
        id: Set(event_id),
        acknowledged_at: Set(None),
        acknowledged_by: Set(None),
        updated_at: Set(Utc::now()),
        ..Default::default()
    }
    .update(&state.db)
    .await?;

    Ok(StatusCode::NO_CONTENT)
}
/// Get a summary of active alarm violations
///
/// Returns counts by severity and by site.
#[utoipa::path(
    get,
    path = "/api/alarms/summary",
    responses(
        (status = 200, description = "Alarm summary", body = AlarmSummaryResponse),
    ),
    tag = "alarms"
)]
pub async fn get_alarm_summary(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<AlarmSummaryResponse>> {
    let mut rows = fetch_active_alarm_rows(&state.db, &scope, None, false).await?;
    rows.extend(fetch_active_alarm_rows(&state.db, &scope, None, true).await?);

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

    let latest_by_site = fetch_latest_reading_times(&state.db, &scope).await?;
    let event_times_by_site = fetch_last_alarm_warning_times(&state.db, &scope).await?;

    let mut covered_sites: HashSet<Uuid> = site_map.keys().copied().collect();
    let mut by_site: Vec<AlarmSiteSummary> = site_map
        .into_iter()
        .map(|(site_id, (site_name, warnings, alarms))| {
            let (last_warning_at, last_alarm_at) = event_times_by_site
                .get(&site_id)
                .copied()
                .unwrap_or((None, None));
            AlarmSiteSummary {
                site_id,
                site_name,
                warning_count: warnings,
                alarm_count: alarms,
                latest_reading_time: latest_by_site.get(&site_id).map(|(_, t)| *t),
                last_warning_at,
                last_alarm_at,
            }
        })
        .collect();

    for (site_id, (site_name, latest_time)) in &latest_by_site {
        if covered_sites.insert(*site_id) {
            let (last_warning_at, last_alarm_at) = event_times_by_site
                .get(site_id)
                .copied()
                .unwrap_or((None, None));
            by_site.push(AlarmSiteSummary {
                site_id: *site_id,
                site_name: site_name.clone(),
                warning_count: 0,
                alarm_count: 0,
                latest_reading_time: Some(*latest_time),
                last_warning_at,
                last_alarm_at,
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
/// List persisted alarm events
///
/// Returns rows from `alarm_events` (the stateful breach history), filterable by site, severity
/// (`max_severity`), and lifecycle status (`open`/`resolved`/`all`). Ordered most-recently-seen first.
#[utoipa::path(
    get,
    path = "/api/alarms/events",
    params(AlarmEventsQuery),
    responses(
        (status = 200, description = "Persisted alarm events", body = AlarmEventsResponse),
    ),
    tag = "alarms"
)]
pub async fn get_alarm_events(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(query): Query<AlarmEventsQuery>,
) -> AppResult<Json<AlarmEventsResponse>> {
    let limit = Window::from_limit_offset(query.limit, None, 200, 1000).limit;

    let mut values: Vec<sea_orm::Value> = Vec::new();
    let mut conditions: Vec<String> = Vec::new();

    if let Some(predicate) = project_filter_sql(&scope, "s.project_id", &mut values) {
        conditions.push(predicate);
    }
    if let Some(site_id) = query.site_id {
        values.push(site_id.into());
        conditions.push(format!("ae.site_id = ${}", values.len()));
    }
    if let Some(severity) = query.severity {
        values.push(severity.into());
        conditions.push(format!("ae.max_severity = ${}", values.len()));
    }
    if let Some(parameter_id) = query.parameter_id {
        values.push(parameter_id.into());
        conditions.push(format!("ae.parameter_id = ${}", values.len()));
    }
    if let Some(start) = query.start {
        values.push(start.into());
        conditions.push(format!("ae.last_seen_at >= ${}", values.len()));
    }
    if let Some(end) = query.end {
        values.push(end.into());
        conditions.push(format!("ae.started_at <= ${}", values.len()));
    }
    match query.status.as_deref() {
        Some("open") => conditions.push("ae.resolved_at IS NULL".to_string()),
        Some("resolved") => conditions.push("ae.resolved_at IS NOT NULL".to_string()),
        _ => {}
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let offset = query.offset.unwrap_or(0);

    let sql = format!(
        r"
        SELECT ae.id, ae.site_id, s.name AS site_name, ae.parameter_id,
               COALESCE(sp.name, p.name) AS parameter_name, ae.measurement_type,
               ae.severity, ae.max_severity, ae.started_at, ae.last_seen_at,
               ae.value_at_start, ae.last_value, ae.resolved_at, ae.resolved_value,
               ae.acknowledged_at, ae.acknowledged_by
        FROM alarm_events ae
        JOIN sites s ON s.id = ae.site_id
        JOIN parameters p ON p.id = ae.parameter_id
        LEFT JOIN site_parameters sp ON sp.site_id = ae.site_id AND sp.parameter_id = ae.parameter_id
        {where_clause}
        ORDER BY (ae.resolved_at IS NULL) DESC, ae.last_seen_at DESC
        LIMIT {limit} OFFSET {offset}
        "
    );

    // Count all matching events (before LIMIT) so `total` reflects the full match set, not the
    // truncated page. Only the alarm_events + sites join is needed, no filter touches sp/p.
    let count_sql = format!(
        r"
        SELECT COUNT(*) AS cnt
        FROM alarm_events ae
        JOIN sites s ON s.id = ae.site_id
        {where_clause}
        "
    );
    let total: usize = state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &count_sql,
            values.clone(),
        ))
        .await?
        .map(|row| row.try_get::<i64>("", "cnt"))
        .transpose()?
        .unwrap_or(0)
        .max(0) as usize;

    let events: Vec<AlarmEventResponse> = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?
        .into_iter()
        .filter_map(|row| AlarmEventRow::from_query_result(&row, "").ok())
        .map(|r| AlarmEventResponse {
            id: r.id,
            site_id: r.site_id,
            site_name: r.site_name,
            parameter_id: r.parameter_id,
            parameter_name: r.parameter_name,
            measurement_type: r.measurement_type,
            severity: r.severity,
            max_severity: r.max_severity,
            started_at: r.started_at.with_timezone(&Utc),
            last_seen_at: r.last_seen_at.with_timezone(&Utc),
            value_at_start: r.value_at_start,
            last_value: r.last_value,
            resolved_at: r.resolved_at.map(|t| t.with_timezone(&Utc)),
            resolved_value: r.resolved_value,
            acknowledged_at: r.acknowledged_at.map(|t| t.with_timezone(&Utc)),
            acknowledged_by: r.acknowledged_by,
        })
        .collect();

    Ok(Json(AlarmEventsResponse { events, total }))
}
/// Resolved thresholds, one row per active `(site, parameter)` slot, each carrying its latest value.
///
/// The single source of truth for the 3-tier resolution (site row → global row → parameter
/// default), built by `engine::resolve_thresholds_query`. The UI consumes this instead of
/// re-deriving the tiers client-side. Optional `site_id` / `parameter_id` scope.
#[utoipa::path(
    get,
    path = "/api/alarms/thresholds",
    params(ThresholdsQuery),
    responses((
        status = 200,
        description = "Resolved thresholds + current value per (site, parameter)",
        body = [ThresholdWithValue]
    )),
    tag = "alarms"
)]
pub async fn get_thresholds(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(query): Query<ThresholdsQuery>,
) -> AppResult<Json<Vec<ThresholdWithValue>>> {
    let resolved_cte = super::service::resolve_thresholds_query(
        query.site_id,
        query.parameter_id.map(|p| vec![p]),
    )
    .to_string(sea_orm::sea_query::PostgresQueryBuilder);

    // Confined to the caller's projects, the same rule the three alarm siblings apply: the payload
    // is one row per active slot, so an unconfined answer is an inventory of every project's slots
    // and their current values.
    let mut values: Vec<sea_orm::Value> = Vec::new();
    let project_filter = project_filter_sql(&scope, "s.project_id", &mut values)
        .map(|predicate| format!("WHERE {predicate}"))
        .unwrap_or_default();

    // Attach the latest reading per slot so the table can show a current value beside each threshold.
    // Bounded to the last 30 days so TimescaleDB chunk-excludes to recent chunks (fast even for the
    // unscoped/global view); a slot with no recent reading gets a NULL current value. Continuous
    // readings win over spot so an occasional grab does not stand in for a sensor's current value;
    // a spot-only slot still reports its latest grab.
    let latest_cte = super::service::latest_slot_values_sql();
    let sql = format!(
        "WITH resolved AS ({resolved_cte}), latest AS ({latest_cte}) \
         SELECT r.site_id, r.parameter_id, r.warning_min, r.warning_max, r.alarm_min, r.alarm_max, \
                r.source, l.current_value \
         FROM resolved r \
         JOIN sites s ON s.id = r.site_id \
         LEFT JOIN latest l ON l.site_id = r.site_id AND l.parameter_id = r.parameter_id \
         {project_filter}"
    );

    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?
        .into_iter()
        .filter_map(|r| ThresholdWithValue::from_query_result(&r, "").ok())
        .collect();

    Ok(Json(rows))
}
/// Run the reconcile every hook of this request asked for, once, after the handler returns.
pub async fn coalesce_reconcile(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let owed = Arc::new(AtomicBool::new(false));
    let response = super::flows::RECONCILE_OWED
        .scope(owed.clone(), next.run(request))
        .await;
    if owed.load(Ordering::Relaxed) {
        super::flows::reconcile_all_now(&state.db).await;
    }
    response
}

/// The alarm surface a reader asks for: what is breaching now, the rollup of it, the episode
/// history and the thresholds those are judged against.
///
/// `/sites/{id}/alarms` is not here. It is the per-site time series, addressed under the site
/// rather than under the alarm, and it stays with the other cross-cutting site routes (Q143).
pub fn read_routes() -> Router<AppState> {
    Router::new()
        .route("/alarms/active", get(get_active_alarms))
        .route("/alarms/summary", get(get_alarm_summary))
        .route("/alarms/events", get(get_alarm_events))
        .route("/alarms/thresholds", get(get_thresholds))
        .layer(middleware::from_fn(
            crate::common::middleware::require_read_data,
        ))
}

/// Acknowledging an episode, and taking that back.
///
/// `deny_scoped_token` alongside the write gate: an acknowledgement is not confined to a project,
/// so a project-scoped token has no business making one.
pub fn write_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/alarms/{event_id}/acknowledge",
            post(acknowledge_alarm).delete(unacknowledge_alarm),
        )
        .layer(middleware::from_fn(
            crate::common::middleware::deny_scoped_token,
        ))
        .layer(middleware::from_fn(
            crate::common::middleware::require_write_data,
        ))
}

/// What one reconciliation pass changed.
#[derive(Debug, Serialize, ToSchema)]
pub struct ReconcileAlarmsResponse {
    pub opened: usize,
    pub updated: usize,
    pub resolved: usize,
}

#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct RebuildAlarmEventsRequest {
    /// Restrict to one site (default: every active site).
    #[serde(default)]
    pub site_id: Option<Uuid>,
    /// Restrict to one parameter (default: every parameter at the targeted sites).
    #[serde(default)]
    pub parameter_id: Option<Uuid>,
    /// Window start (ISO 8601). Defaults per-slot to the slot's earliest reading.
    #[serde(default)]
    pub start: Option<chrono::DateTime<chrono::Utc>>,
    /// Window end (ISO 8601). Defaults per-slot to the slot's latest reading.
    #[serde(default)]
    pub end: Option<chrono::DateTime<chrono::Utc>>,
}

/// Reconstruct persisted alarm events from the actual readings, for the targeted slots and window.
/// Walks the readings, collapses consecutive out-of-range readings into resolved breach episodes,
/// and writes them to `alarm_events` (idempotently). This is the on-demand twin of the automatic
/// backfill that fires after a CSV import / batch ingest; the live 60s sweeper still owns currently
/// open breaches. Tracked as a `reprocessing_jobs` row (`trigger_type = 'alarm_backfill'`); returns
/// the job id immediately. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/rebuild_alarm_events",
    request_body = RebuildAlarmEventsRequest,
    responses(
        (status = 200, description = "Rebuild triggered", body = QueuedJobResponse),
        (status = 403, description = "The named site is outside the caller's projects, or no site was named"),
    ),
    tag = "actions"
)]
pub async fn rebuild_alarm_events(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<RebuildAlarmEventsRequest>,
) -> AppResult<Json<QueuedJobResponse>> {
    let RebuildAlarmEventsRequest {
        site_id,
        parameter_id,
        start,
        end,
    } = payload;

    require_named_target(&scope, site_id.is_some(), "site")?;
    if let Some(site_id) = site_id {
        require_sites_in_scope(&app_state.db, &scope, &[site_id]).await?;
    }

    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
        &app_state.db,
        "alarm_backfill",
        None,
        None,
        &serde_json::json!({
            "site_id": site_id,
            "parameter_id": parameter_id,
            "start": start,
            "end": end,
        }),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(QueuedJobResponse::queued(job_id)))
}

/// Force a full open-alarm reconcile right now, instead of waiting for the periodic backstop
/// sweep. Runs the same single tick the sweeper runs (open new breaches, refresh still-breaching,
/// auto-resolve returned-to-range) across every active slot, synchronously, the post-LATERAL
/// breach query is O(active slots), so this returns in well under a second. Operator escape hatch
/// for "I changed something and want the alarm state correct immediately". Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/reconcile_alarms",
    responses(
        (
            status = 200,
            description = "Reconcile complete; counts of opened/updated/resolved events",
            body = ReconcileAlarmsResponse
        ),
    ),
    tag = "actions"
)]
pub async fn reconcile_alarms(
    State(app_state): State<AppState>,
) -> AppResult<Json<ReconcileAlarmsResponse>> {
    let stats = crate::routes::private::alarms::flows::evaluate_alarm_events(&app_state.db).await?;

    if stats.opened > 0 || stats.resolved > 0 {
        let _ = app_state
            .events
            .send(crate::common::AppEvent::AlarmStateChanged {
                opened: stats.opened,
                resolved: stats.resolved,
            });
    }

    Ok(Json(ReconcileAlarmsResponse {
        opened: stats.opened,
        updated: stats.updated,
        resolved: stats.resolved,
    }))
}
