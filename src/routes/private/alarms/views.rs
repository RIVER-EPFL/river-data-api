use axum::{
    Json,
    extract::{Path, Query, State},
    http::{StatusCode, header::HeaderMap},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, QueryOrder, Statement,
};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::{AuthContext, ProjectScope};
use crate::common::scope::{Unowned, project_filter_sql, project_of_alarm_event, require_row_in_scope};
use crate::common::series::{self, Cells, Table};
use crate::routes::private::sites::parameters as site_parameters;
use crate::error::{AppError, AppResult};
use crate::routes::{cache, resolve_site_with_project, validate_time_range};
use crate::common::bulk;

use super::types::{
    AcknowledgedAlarmResponse, ActiveAlarm, ActiveAlarmsResponse, AlarmEventResponse,
    AlarmEventsQuery, AlarmEventsResponse, AlarmSeverityCounts, AlarmSiteSummary,
    AlarmSummaryResponse, AlarmThresholdInfo, AlarmViolationsResponse, ParameterViolationData,
    SiteAlarmsQuery,
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

    let min_severity = query.severity.unwrap_or(1);
    let val_expr = "COALESCE(smp.mean, r.calibrated_value, r.raw_value)";

    let violation_condition = super::thresholds::violation_condition(
        val_expr,
        "t.warning_min",
        "t.warning_max",
        "t.alarm_min",
        "t.alarm_max",
        min_severity,
    );
    let sev_case = super::thresholds::severity_case(
        val_expr,
        "t.warning_min",
        "t.warning_max",
        "t.alarm_min",
        "t.alarm_max",
    );

    // The single resolution definition (site → global → parameter default), scoped to this site and
    // spliced as the threshold CTE. Param ids are inlined in the rendered CTE, so the outer query
    // only binds $1 = site_id, $2 = start, $3 = end.
    let resolved_cte =
        super::thresholds::resolve_thresholds_sql(Some(site.id), Some(alarm_param_ids));

    let sql = format!(
        r"
        WITH resolved_thresholds AS ({resolved_cte})
        SELECT
            r.parameter_id,
            r.time,
            COALESCE(smp.mean, r.calibrated_value, r.raw_value) AS value,
            ({sev_case})::smallint as severity
        FROM readings r
        LEFT JOIN samples smp ON smp.id = r.sample_id
        JOIN resolved_thresholds t ON r.parameter_id = t.parameter_id
        WHERE r.site_id = $1
          AND r.time >= $2
          AND r.time <= $3
          AND r.replicate_index = 0
          AND {violation_condition}
        ORDER BY r.time, r.parameter_id
        "
    );

    let values: Vec<sea_orm::Value> = vec![site.id.into(), query.start.into(), query.end.into()];

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

/// Cadence lens for readings queries: the grab (spot) series or everything else
/// (continuous and derived; NULL reads as continuous).
pub(crate) fn cadence_predicate(spot: bool) -> &'static str {
    if spot {
        "r.measurement_type = 'spot'"
    } else {
        "r.measurement_type IS DISTINCT FROM 'spot'"
    }
}

pub(crate) fn cadence_label(spot: bool) -> &'static str {
    if spot { "spot" } else { "continuous" }
}

/// Row from the active alarms query
#[derive(Debug, FromQueryResult)]
pub(crate) struct ActiveAlarmRow {
    pub(crate) site_id: Uuid,
    pub(crate) site_name: String,
    pub(crate) parameter_id: Uuid,
    pub(crate) parameter_name: String,
    pub(crate) current_value: f64,
    pub(crate) time: chrono::DateTime<chrono::FixedOffset>,
    pub(crate) warning_min: Option<f64>,
    pub(crate) warning_max: Option<f64>,
    pub(crate) alarm_min: Option<f64>,
    pub(crate) alarm_max: Option<f64>,
    pub(crate) severity: i16,
}

/// Fetch active alarm violations across all sites. The sweeper reuses this as the "current breach
/// set" so the persisted events never diverge from what `/alarms/active` would compute.
/// `spot` selects the cadence lens: grab series and sensor series are evaluated independently,
/// so a monthly grab can neither mask nor phantom-resolve a sensor breach.
pub(crate) async fn fetch_active_alarm_rows(
    db: &sea_orm::DatabaseConnection,
    scope: &crate::common::authz::AccessScope,
    slots: Option<&[(Uuid, Uuid)]>,
    spot: bool,
) -> AppResult<Vec<ActiveAlarmRow>> {
    // Empty slot list means "evaluate nothing", short-circuit before building an invalid `IN ()`.
    if matches!(slots, Some(s) if s.is_empty()) {
        return Ok(Vec::new());
    }

    let sev_case = super::thresholds::severity_case(
        "lr.value",
        "rt.warning_min",
        "rt.warning_max",
        "rt.alarm_min",
        "rt.alarm_max",
    );
    let violation = super::thresholds::violation_condition(
        "lr.value",
        "rt.warning_min",
        "rt.warning_max",
        "rt.alarm_min",
        "rt.alarm_max",
        1,
    );

    // The single resolution definition across all active slots (no scope), spliced as the CTE.
    let resolved_cte = super::thresholds::resolve_thresholds_sql(None, None);

    let cadence = cadence_predicate(spot);

    // Bind params are appended in order: optional project scope, then optional (site, parameter)
    // slot pairs. `next` tracks the next `$N` placeholder.
    let mut values: Vec<sea_orm::Value> = Vec::new();

    let project_filter = project_filter_sql(scope, "s.project_id", &mut values)
        .map(|predicate| format!("AND {predicate}"))
        .unwrap_or_default();
    let mut next = values.len() + 1;

    let slot_filter = if let Some(slots) = slots {
        let mut pairs = Vec::with_capacity(slots.len());
        for (site_id, parameter_id) in slots {
            pairs.push(format!("(${},${})", next, next + 1));
            values.push((*site_id).into());
            values.push((*parameter_id).into());
            next += 2;
        }
        format!("AND (rt.site_id, rt.parameter_id) IN ({})", pairs.join(","))
    } else {
        String::new()
    };

    // Loose index scan: one `ORDER BY time DESC LIMIT 1` per active slot via
    // `idx_readings_site_param_time`, instead of a `DISTINCT ON` over the whole hypertable. Cost is
    // O(active slots), independent of history depth.
    //
    // `replicate_index = 0` keeps the "latest value" deterministic when grab replicates share a
    // timestamp, matching episodes and the continuous aggregates. Flagged readings deliberately
    // still drive alarms (divergent from aggregates/samples, which exclude them): an out-of-range
    // value should keep alerting even after someone flags it.
    let sql = format!(
        r"
        WITH resolved_thresholds AS ({resolved_cte})
        SELECT
            rt.site_id,
            s.name AS site_name,
            rt.parameter_id,
            sp.name AS parameter_name,
            lr.value AS current_value,
            lr.time,
            rt.warning_min,
            rt.warning_max,
            rt.alarm_min,
            rt.alarm_max,
            ({sev_case})::smallint AS severity
        FROM resolved_thresholds rt
        JOIN sites s ON s.id = rt.site_id
        JOIN site_parameters sp
            ON sp.site_id = rt.site_id
            AND sp.parameter_id = rt.parameter_id
            AND sp.is_active = true
        CROSS JOIN LATERAL (
            SELECT COALESCE(smp.mean, r.calibrated_value, r.raw_value) AS value, r.time
            FROM readings r
            LEFT JOIN samples smp ON smp.id = r.sample_id
            WHERE r.site_id = rt.site_id AND r.parameter_id = rt.parameter_id
              AND r.replicate_index = 0
              AND {cadence}
            ORDER BY r.time DESC
            LIMIT 1
        ) lr
        WHERE {violation}
        {project_filter}
        {slot_filter}
        ORDER BY severity DESC, s.name, parameter_name
        "
    );

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

/// Open persisted alarm event, keyed by (site, parameter, measurement_type) for annotating the
/// live feed.
#[derive(Debug, FromQueryResult)]
struct OpenEventRow {
    site_id: Uuid,
    parameter_id: Uuid,
    measurement_type: String,
    id: Uuid,
    started_at: chrono::DateTime<chrono::FixedOffset>,
    acknowledged_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    acknowledged_by: Option<String>,
    max_severity: i16,
}

/// Fetch the currently-open alarm events as a map keyed by (site_id, parameter_id,
/// measurement_type). Used to attach `event_id` + acknowledgement state to the (stateless)
/// current-breach feed.
async fn fetch_open_events(
    db: &sea_orm::DatabaseConnection,
    scope: &crate::common::authz::AccessScope,
) -> AppResult<HashMap<(Uuid, Uuid, String), OpenEventRow>> {
    let mut values: Vec<sea_orm::Value> = Vec::new();
    let project_filter = project_filter_sql(scope, "s.project_id", &mut values)
        .map(|predicate| format!("AND {predicate}"))
        .unwrap_or_default();
    let sql = format!(
        "SELECT ae.site_id, ae.parameter_id, ae.measurement_type, ae.id, ae.started_at, ae.acknowledged_at, ae.acknowledged_by, ae.max_severity \
         FROM alarm_events ae JOIN sites s ON s.id = ae.site_id \
         WHERE ae.resolved_at IS NULL {project_filter}"
    );
    let mut map = HashMap::new();
    for row in db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?
    {
        if let Ok(r) = OpenEventRow::from_query_result(&row, "") {
            map.insert((r.site_id, r.parameter_id, r.measurement_type.clone()), r);
        }
    }
    Ok(map)
}

/// Get currently active alarm violations across all sites
///
/// For each alarm threshold, checks the latest reading to see if it violates warning or alarm
/// limits. Each current violation is annotated with its persisted `alarm_event` (id + acknowledgement
/// state) so the UI can acknowledge it; the breach set itself stays driven by the latest readings.
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
                site_id: row.site_id,
                site_name: row.site_name,
                parameter_id: row.parameter_id,
                parameter_name: row.parameter_name,
                current_value: row.current_value,
                measurement_type: cadence.to_string(),
                threshold: AlarmThresholdInfo {
                    warning_min: row.warning_min,
                    warning_max: row.warning_max,
                    alarm_min: row.alarm_min,
                    alarm_max: row.alarm_max,
                },
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

/// Confine an id-addressed alarm event to the caller's projects. An event outside them and an event
/// that does not exist answer identically, so acknowledging by id cannot be used to discover that
/// another project's alarm exists. The listing (`/alarms/events`) is filtered by the same rule.
async fn confine_alarm_event(
    state: &AppState,
    scope: &crate::common::authz::AccessScope,
    event_id: Uuid,
) -> AppResult<()> {
    let row = project_of_alarm_event(&state.db, event_id).await?;
    require_row_in_scope(scope, &row, Unowned::Deny, "Alarm event")
}

/// Best-effort actor identity for the `acknowledged_by` audit field.
fn actor_label(auth: &AuthContext) -> String {
    match auth {
        AuthContext::Keycloak { email: Some(e), .. } => e.clone(),
        AuthContext::Keycloak { .. } => "keycloak".to_string(),
        AuthContext::ApiToken { token_id, .. } => format!("token:{token_id}"),
    }
}

/// Acknowledge an open alarm event
///
/// Marks the open `alarm_event` as acknowledged by the calling user/token. Acknowledging does not
/// resolve the alarm; it stays active (flagged `acknowledged: true`) until the reading returns to
/// range. Returns 404 if the event does not exist or sits outside the caller's projects, 409 if it
/// is already resolved.
#[utoipa::path(
    post,
    path = "/alarms/{event_id}/acknowledge",
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
    #[derive(Debug, FromQueryResult)]
    struct EventState {
        resolved_at: Option<chrono::DateTime<chrono::FixedOffset>>,
        acknowledged_at: Option<chrono::DateTime<chrono::FixedOffset>>,
        acknowledged_by: Option<String>,
    }

    confine_alarm_event(&state, &scope, event_id).await?;

    let existing = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT resolved_at, acknowledged_at, acknowledged_by FROM alarm_events WHERE id = $1",
            [event_id.into()],
        ))
        .await?
        .and_then(|r| EventState::from_query_result(&r, "").ok());

    let Some(ev) = existing else {
        return Err(AppError::NotFound(format!("Alarm event {event_id} not found")));
    };
    if ev.resolved_at.is_some() {
        return Err(AppError::Conflict("Alarm already resolved".to_string()));
    }
    // Idempotent: an already-acknowledged open event returns its existing acknowledgement.
    if let (Some(at), Some(by)) = (ev.acknowledged_at, ev.acknowledged_by) {
        return Ok(Json(AcknowledgedAlarmResponse {
            event_id,
            acknowledged_at: at.with_timezone(&Utc),
            acknowledged_by: by,
        }));
    }

    let actor = actor_label(&auth);
    let row = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE alarm_events SET acknowledged_at = NOW(), acknowledged_by = $2, updated_at = NOW() \
             WHERE id = $1 AND resolved_at IS NULL RETURNING acknowledged_at",
            [event_id.into(), actor.clone().into()],
        ))
        .await?;
    let acknowledged_at: chrono::DateTime<chrono::FixedOffset> = row
        .and_then(|r| r.try_get("", "acknowledged_at").ok())
        .ok_or_else(|| AppError::Conflict("Alarm already resolved".to_string()))?;

    Ok(Json(AcknowledgedAlarmResponse {
        event_id,
        acknowledged_at: acknowledged_at.with_timezone(&Utc),
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
    path = "/alarms/{event_id}/acknowledge",
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
    #[derive(Debug, FromQueryResult)]
    struct EventCheck {
        resolved_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    }

    confine_alarm_event(&state, &scope, event_id).await?;

    let existing = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT resolved_at FROM alarm_events WHERE id = $1",
            [event_id.into()],
        ))
        .await?
        .and_then(|r| EventCheck::from_query_result(&r, "").ok());

    let Some(ev) = existing else {
        return Err(AppError::NotFound(format!("Alarm event {event_id} not found")));
    };
    if ev.resolved_at.is_some() {
        return Err(AppError::Conflict("Alarm already resolved".to_string()));
    }

    state
        .db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE alarm_events SET acknowledged_at = NULL, acknowledged_by = NULL, updated_at = NOW() \
             WHERE id = $1 AND resolved_at IS NULL",
            [event_id.into()],
        ))
        .await?;

    Ok(StatusCode::NO_CONTENT)
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
    scope: &crate::common::authz::AccessScope,
) -> AppResult<HashMap<Uuid, (String, DateTime<Utc>)>> {
    let mut values: Vec<sea_orm::Value> = Vec::new();
    let project_filter = project_filter_sql(scope, "s.project_id", &mut values)
        .map(|predicate| format!("WHERE {predicate}"))
        .unwrap_or_default();

    // Continuous-only: a monthly grab must not make a dead logger look alive.
    let sql = format!(
        r"
        SELECT s.id AS site_id, s.name AS site_name, MAX(r.time) AS latest_time
        FROM sites s
        JOIN readings r ON r.site_id = s.id AND r.measurement_type IS DISTINCT FROM 'spot'
        {project_filter}
        GROUP BY s.id, s.name
        "
    );

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

/// Row from the per-site last warning/alarm event query
#[derive(Debug, FromQueryResult)]
struct LastAlarmWarningRow {
    site_id: Uuid,
    last_warning_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    last_alarm_at: Option<chrono::DateTime<chrono::FixedOffset>>,
}

/// Fetch per-site last persisted warning (max_severity = 1) and alarm (max_severity = 2) timestamps
/// from `alarm_events`, keyed by site_id as `(last_warning_at, last_alarm_at)`.
async fn fetch_last_alarm_warning_times(
    db: &sea_orm::DatabaseConnection,
    scope: &crate::common::authz::AccessScope,
) -> AppResult<HashMap<Uuid, (Option<DateTime<Utc>>, Option<DateTime<Utc>>)>> {
    let mut values: Vec<sea_orm::Value> = Vec::new();
    let project_filter = project_filter_sql(scope, "s.project_id", &mut values)
        .map(|predicate| format!("WHERE {predicate}"))
        .unwrap_or_default();

    let sql = format!(
        r"
        SELECT s.id AS site_id,
               MAX(ae.last_seen_at) FILTER (WHERE ae.max_severity = 1) AS last_warning_at,
               MAX(ae.last_seen_at) FILTER (WHERE ae.max_severity = 2) AS last_alarm_at
        FROM alarm_events ae
        JOIN sites s ON s.id = ae.site_id
        {project_filter}
        GROUP BY s.id
        "
    );

    let rows: Vec<LastAlarmWarningRow> = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?
        .into_iter()
        .filter_map(|row| LastAlarmWarningRow::from_query_result(&row, "").ok())
        .collect();

    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.site_id,
                (
                    r.last_warning_at.map(|t| t.with_timezone(&Utc)),
                    r.last_alarm_at.map(|t| t.with_timezone(&Utc)),
                ),
            )
        })
        .collect())
}

/// Row from the persisted alarm-events feed query
#[derive(Debug, FromQueryResult)]
struct AlarmEventRow {
    id: Uuid,
    site_id: Uuid,
    site_name: String,
    parameter_id: Uuid,
    parameter_name: String,
    measurement_type: String,
    severity: i16,
    max_severity: i16,
    started_at: chrono::DateTime<chrono::FixedOffset>,
    last_seen_at: chrono::DateTime<chrono::FixedOffset>,
    value_at_start: f64,
    last_value: f64,
    resolved_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    resolved_value: Option<f64>,
    acknowledged_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    acknowledged_by: Option<String>,
}

/// List persisted alarm events
///
/// Returns rows from `alarm_events` (the stateful breach history), filterable by site, severity
/// (`max_severity`), and lifecycle status (`open`/`resolved`/`all`). Ordered most-recently-seen first.
#[utoipa::path(
    get,
    path = "/alarms/events",
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
    let limit = query.limit.unwrap_or(200).min(1000);

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
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &count_sql,
            values.clone(),
        ))
        .await?
        .and_then(|row| row.try_get::<i64>("", "cnt").ok())
        .unwrap_or(0)
        .max(0) as usize;

    let events: Vec<AlarmEventResponse> = state
        .db
        .query_all(Statement::from_sql_and_values(
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

/// Query for the resolved-thresholds feed.
#[derive(Debug, serde::Deserialize, utoipa::IntoParams)]
pub struct ThresholdsQuery {
    pub site_id: Option<Uuid>,
    pub parameter_id: Option<Uuid>,
}

/// One resolved threshold plus the slot's latest reading value, for the UI thresholds table.
#[derive(FromQueryResult, serde::Serialize, utoipa::ToSchema)]
pub struct ThresholdWithValue {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub warning_min: Option<f64>,
    pub warning_max: Option<f64>,
    pub alarm_min: Option<f64>,
    pub alarm_max: Option<f64>,
    pub source: String,
    /// Latest reading (last 30 days) for this slot, or null if none, display only.
    pub current_value: Option<f64>,
}

/// Resolved thresholds, one row per active `(site, parameter)` slot, each carrying its latest value.
///
/// The single source of truth for the 3-tier resolution (site row → global row → parameter
/// default), built by `engine::resolve_thresholds_query`. The UI consumes this instead of
/// re-deriving the tiers client-side. Optional `site_id` / `parameter_id` scope.
#[utoipa::path(
    get,
    path = "/alarms/thresholds",
    params(ThresholdsQuery),
    responses((status = 200, description = "Resolved thresholds + current value per (site, parameter)")),
    tag = "alarms"
)]
pub async fn get_thresholds(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(query): Query<ThresholdsQuery>,
) -> AppResult<Json<Vec<ThresholdWithValue>>> {
    let resolved_cte = super::thresholds::resolve_thresholds_sql(
        query.site_id,
        query.parameter_id.map(|p| vec![p]),
    );

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
    let sql = format!(
        "WITH resolved AS ({resolved_cte}), \
         latest AS ( \
            SELECT DISTINCT ON (r.site_id, r.parameter_id) r.site_id, r.parameter_id, \
                   COALESCE(smp.mean, r.calibrated_value, r.raw_value) AS current_value \
            FROM readings r \
            LEFT JOIN samples smp ON smp.id = r.sample_id \
            WHERE r.site_id IS NOT NULL AND r.is_flagged IS NOT TRUE \
              AND r.time > now() - interval '30 days' \
            ORDER BY r.site_id, r.parameter_id, \
                     (r.measurement_type IS NOT DISTINCT FROM 'spot') ASC, r.time DESC, \
                     r.replicate_index ASC \
         ) \
         SELECT r.site_id, r.parameter_id, r.warning_min, r.warning_max, r.alarm_min, r.alarm_max, \
                r.source, l.current_value \
         FROM resolved r \
         JOIN sites s ON s.id = r.site_id \
         LEFT JOIN latest l ON l.site_id = r.site_id AND l.parameter_id = r.parameter_id \
         {project_filter}"
    );

    let rows = state
        .db
        .query_all(Statement::from_sql_and_values(
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
