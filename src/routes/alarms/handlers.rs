use axum::{
    extract::{Path, Query, State},
    Json,
};
use chrono::Utc;
use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::{alarm_locations, alarms, events};
use crate::error::{AppError, AppResult};
use crate::routes::resolve_station;

use super::types::{
    AlarmResponse, AlarmSummary, AlarmsQuery, EventResponse, EventsListResponse, EventsQuery,
};

/// List alarms with optional filtering
#[utoipa::path(
    get,
    path = "/api/alarms",
    params(AlarmsQuery),
    responses(
        (status = 200, description = "Alarms retrieved successfully", body = Vec<AlarmSummary>),
    ),
    tag = "alarms"
)]
pub async fn list_alarms(
    State(state): State<AppState>,
    Query(query): Query<AlarmsQuery>,
) -> AppResult<Json<Vec<AlarmSummary>>> {
    let mut db_query = alarms::Entity::find();

    // Filter by active status
    if let Some(active) = query.active {
        db_query = db_query.filter(alarms::Column::Status.eq(active));
    }

    // Filter by severity
    if let Some(severity) = query.severity {
        db_query = db_query.filter(alarms::Column::Severity.eq(severity));
    }

    // Filter by time range
    if let Some(start) = query.start {
        db_query = db_query.filter(alarms::Column::WhenOn.gte(start));
    }
    if let Some(end) = query.end {
        db_query = db_query.filter(alarms::Column::WhenOn.lte(end));
    }

    // Filter by station using the direct station_id column
    if let Some(station_id_str) = &query.station_id {
        let station = resolve_station(&state.db, station_id_str).await?;
        db_query = db_query.filter(alarms::Column::StationId.eq(station.id));
    }

    let alarms_list = db_query
        .order_by_desc(alarms::Column::WhenOn)
        .all(&state.db)
        .await?;

    let response: Vec<AlarmSummary> = alarms_list
        .into_iter()
        .map(|a| {
            let duration = format_duration(a.duration_sec);
            AlarmSummary {
                id: a.id,
                severity: a.severity,
                description: a.description,
                when_on: a.when_on.with_timezone(&Utc),
                when_off: a.when_off.map(|t| t.with_timezone(&Utc)),
                status: a.status,
                is_system: a.is_system,
                location_text: a.location_text,
                station_id: a.station_id,
                duration,
            }
        })
        .collect();

    Ok(Json(response))
}

/// List only active alarms
#[utoipa::path(
    get,
    path = "/api/alarms/active",
    responses(
        (status = 200, description = "Active alarms retrieved successfully", body = Vec<AlarmSummary>),
    ),
    tag = "alarms"
)]
pub async fn list_active_alarms(State(state): State<AppState>) -> AppResult<Json<Vec<AlarmSummary>>> {
    let alarms_list = alarms::Entity::find()
        .filter(alarms::Column::Status.eq(true))
        .order_by_desc(alarms::Column::WhenOn)
        .all(&state.db)
        .await?;

    let response: Vec<AlarmSummary> = alarms_list
        .into_iter()
        .map(|a| {
            let duration = format_duration(a.duration_sec);
            AlarmSummary {
                id: a.id,
                severity: a.severity,
                description: a.description,
                when_on: a.when_on.with_timezone(&Utc),
                when_off: a.when_off.map(|t| t.with_timezone(&Utc)),
                status: a.status,
                is_system: a.is_system,
                location_text: a.location_text,
                station_id: a.station_id,
                duration,
            }
        })
        .collect();

    Ok(Json(response))
}

/// Get a specific alarm by ID
#[utoipa::path(
    get,
    path = "/api/alarms/{alarm_id}",
    params(
        ("alarm_id" = Uuid, Path, description = "Alarm UUID"),
    ),
    responses(
        (status = 200, description = "Alarm retrieved successfully", body = AlarmResponse),
        (status = 404, description = "Alarm not found"),
    ),
    tag = "alarms"
)]
pub async fn get_alarm(
    State(state): State<AppState>,
    Path(alarm_id): Path<Uuid>,
) -> AppResult<Json<AlarmResponse>> {
    let alarm = alarms::Entity::find_by_id(alarm_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Alarm not found".to_string()))?;

    // Get associated sensor IDs
    let sensor_ids: Vec<Uuid> = alarm_locations::Entity::find()
        .filter(alarm_locations::Column::AlarmId.eq(alarm_id))
        .all(&state.db)
        .await?
        .into_iter()
        .map(|al| al.sensor_id)
        .collect();

    Ok(Json(AlarmResponse {
        id: alarm.id,
        vaisala_alarm_id: alarm.vaisala_alarm_id,
        severity: alarm.severity,
        description: alarm.description,
        error_text: alarm.error_text,
        alarm_type: alarm.alarm_type,
        when_on: alarm.when_on.with_timezone(&Utc),
        when_off: alarm.when_off.map(|t| t.with_timezone(&Utc)),
        when_ack: alarm.when_ack.map(|t| t.with_timezone(&Utc)),
        duration_sec: alarm.duration_sec,
        status: alarm.status,
        is_system: alarm.is_system,
        serial_number: alarm.serial_number,
        location_text: alarm.location_text,
        zone_text: alarm.zone_text,
        station_id: alarm.station_id,
        ack_required: alarm.ack_required,
        sensor_ids,
    }))
}

/// List alarms for a specific station
#[utoipa::path(
    get,
    path = "/api/stations/{station_id}/alarms",
    params(
        ("station_id" = String, Path, description = "Station UUID or name"),
    ),
    responses(
        (status = 200, description = "Station alarms retrieved successfully", body = Vec<AlarmSummary>),
        (status = 404, description = "Station not found"),
    ),
    tag = "alarms"
)]
pub async fn list_station_alarms(
    State(state): State<AppState>,
    Path(station_id): Path<String>,
) -> AppResult<Json<Vec<AlarmSummary>>> {
    let station = resolve_station(&state.db, &station_id).await?;

    // Use the direct station_id column for efficient querying
    let alarms_list = alarms::Entity::find()
        .filter(alarms::Column::StationId.eq(station.id))
        .order_by_desc(alarms::Column::WhenOn)
        .all(&state.db)
        .await?;

    let response: Vec<AlarmSummary> = alarms_list
        .into_iter()
        .map(|a| {
            let duration = format_duration(a.duration_sec);
            AlarmSummary {
                id: a.id,
                severity: a.severity,
                description: a.description,
                when_on: a.when_on.with_timezone(&Utc),
                when_off: a.when_off.map(|t| t.with_timezone(&Utc)),
                status: a.status,
                is_system: a.is_system,
                location_text: a.location_text,
                station_id: a.station_id,
                duration,
            }
        })
        .collect();

    Ok(Json(response))
}

/// List events with filtering and pagination
#[utoipa::path(
    get,
    path = "/api/events",
    params(EventsQuery),
    responses(
        (status = 200, description = "Events retrieved successfully", body = EventsListResponse),
    ),
    tag = "events"
)]
pub async fn list_events(
    State(state): State<AppState>,
    Query(query): Query<EventsQuery>,
) -> AppResult<Json<EventsListResponse>> {
    let mut db_query = events::Entity::find();

    // Required time range filter
    db_query = db_query.filter(events::Column::Time.gte(query.start));
    db_query = db_query.filter(events::Column::Time.lte(query.end));

    // Optional category filter
    if let Some(category) = &query.category {
        db_query = db_query.filter(events::Column::Category.eq(category));
    }

    // Optional station filter using direct station_id column
    if let Some(station_id_str) = &query.station_id {
        let station = resolve_station(&state.db, station_id_str).await?;
        db_query = db_query.filter(events::Column::StationId.eq(station.id));
    }

    // Get total count
    let total = db_query.clone().count(&state.db).await? as i64;

    // Apply pagination and ordering
    let page_size = query.page_size.min(1000).max(1);
    let offset = ((query.page - 1).max(0) * page_size) as u64;

    let events_list = db_query
        .order_by_desc(events::Column::Time)
        .offset(offset)
        .limit(page_size as u64)
        .all(&state.db)
        .await?;

    let events_response: Vec<EventResponse> = events_list
        .into_iter()
        .map(|e| EventResponse {
            time: e.time.with_timezone(&Utc),
            vaisala_event_num: e.vaisala_event_num,
            category: e.category,
            message: e.message,
            user_name: e.user_name,
            entity: e.entity,
            entity_id: e.entity_id,
            sensor_id: e.sensor_id,
            station_id: e.station_id,
            device_id: e.device_id,
        })
        .collect();

    Ok(Json(EventsListResponse {
        events: events_response,
        total,
        page: query.page,
        page_size,
    }))
}

/// Format duration in seconds to human-readable string
fn format_duration(duration_sec: Option<f64>) -> String {
    match duration_sec {
        Some(secs) if secs > 0.0 => {
            let total_secs = secs as i64;
            let days = total_secs / 86400;
            let hours = (total_secs % 86400) / 3600;
            let mins = (total_secs % 3600) / 60;

            if days > 0 {
                format!("{}d {}h {}m", days, hours, mins)
            } else if hours > 0 {
                format!("{}h {}m", hours, mins)
            } else {
                format!("{}m", mins.max(1))
            }
        }
        _ => "ongoing".to_string(),
    }
}
