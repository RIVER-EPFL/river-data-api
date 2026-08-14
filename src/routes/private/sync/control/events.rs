use axum::Json;
use axum::extract::{Path, State};
use chrono::Utc;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use serde::Deserialize;
use uuid::Uuid;

use super::session::SyncServiceContext;
use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::sync::{events_model, services_model};
use river_data_core::models::{SyncEventStatus, SyncEventType};

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateSyncEventRequest {
    pub service_id: Uuid,
    pub command_id: Option<Uuid>,
    pub event_type: Option<String>,
    pub status: Option<String>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct UpdateSyncEventRequest {
    pub status: Option<String>,
    pub readings_synced: Option<i64>,
    /// Connectors on river-data-core 0.5.0 do not send this; absent leaves the stored count alone.
    #[serde(default)]
    pub readings_skipped: Option<i64>,
    pub status_events_synced: Option<i64>,
    #[schema(value_type = Object)]
    pub errors: Option<serde_json::Value>,
    #[schema(value_type = Object)]
    pub log: Option<serde_json::Value>,
    pub duration_ms: Option<i64>,
}

/// Sync service reports the start of a sync operation. Returns the created event ID
/// (used in subsequent `PATCH /events/{id}` calls). Validates event_type and status
/// against the SyncEventType / SyncEventStatus enums. Requires sync session token auth.
#[utoipa::path(
    post,
    path = "/events",
    request_body = CreateSyncEventRequest,
    responses(
        (status = 200, description = "Event created; id and status returned"),
        (status = 400, description = "Invalid event_type or status"),
        (status = 401, description = "Invalid session token"),
        (status = 403, description = "service_id does not match authenticated service"),
    ),
    tag = "sync"
)]
pub async fn create_sync_event(
    State(state): State<AppState>,
    ctx: SyncServiceContext,
    Json(req): Json<CreateSyncEventRequest>,
) -> AppResult<Json<serde_json::Value>> {
    if req.service_id != ctx.service_id {
        return Err(AppError::Forbidden(
            "Event service_id does not match authenticated service".to_string(),
        ));
    }

    if let Some(ref event_type) = req.event_type
        && SyncEventType::from_str(event_type).is_none()
    {
        let valid: Vec<&str> = SyncEventType::ALL.iter().map(|v| v.as_str()).collect();
        return Err(AppError::BadRequest(format!(
            "Invalid event_type '{}'. Valid: {}",
            event_type,
            valid.join(", ")
        )));
    }

    if let Some(ref status) = req.status
        && SyncEventStatus::from_str(status).is_none()
    {
        let valid: Vec<&str> = SyncEventStatus::ALL.iter().map(|v| v.as_str()).collect();
        return Err(AppError::BadRequest(format!(
            "Invalid status '{}'. Valid: {}",
            status,
            valid.join(", ")
        )));
    }

    let event = events_model::ActiveModel {
        id: Set(Uuid::new_v4()),
        service_id: Set(req.service_id),
        command_id: Set(req.command_id),
        event_type: Set(req
            .event_type
            .unwrap_or_else(|| SyncEventType::Scheduled.as_str().to_string())),
        status: Set(req
            .status
            .unwrap_or_else(|| SyncEventStatus::Running.as_str().to_string())),
        readings_synced: Set(0),
        readings_skipped: Set(0),
        status_events_synced: Set(0),
        errors: Set(None),
        log: Set(None),
        started_at: Set(Utc::now().into()),
        completed_at: Set(None),
        duration_ms: Set(None),
    };

    let inserted = event.insert(&state.db).await?;

    Ok(Json(serde_json::json!({
        "id": inserted.id.to_string(),
        "service_id": inserted.service_id,
        "status": inserted.status,
    })))
}

/// Sync service updates an in-progress event with metrics, errors, or completion status.
/// Terminal statuses (`completed`/`failed`) auto-stamp `completed_at`. Successful events
/// also update the owning service's `last_sync_completed_at`. Requires sync session token.
#[utoipa::path(
    patch,
    path = "/events/{id}",
    params(("id" = Uuid, Path, description = "Sync event UUID")),
    request_body = UpdateSyncEventRequest,
    responses(
        (status = 200, description = "Event updated"),
        (status = 401, description = "Invalid session token"),
        (status = 403, description = "Event belongs to a different service"),
        (status = 404, description = "Event not found"),
    ),
    tag = "sync"
)]
pub async fn update_sync_event(
    State(state): State<AppState>,
    ctx: SyncServiceContext,
    Path(event_id): Path<Uuid>,
    Json(req): Json<UpdateSyncEventRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let event = events_model::Entity::find_by_id(event_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Sync event not found".to_string()))?;

    if event.service_id != ctx.service_id {
        return Err(AppError::Forbidden(
            "Event does not belong to this service".to_string(),
        ));
    }

    let service_id = event.service_id;
    let mut active: events_model::ActiveModel = event.into();

    let parsed_status = req.status.as_deref().and_then(SyncEventStatus::from_str);
    let is_terminal = parsed_status.is_some_and(|s| s.is_terminal());
    let is_success = parsed_status.is_some_and(|s| s.is_success());

    if let Some(status) = req.status {
        active.status = Set(status);
    }
    if let Some(readings) = req.readings_synced {
        active.readings_synced = Set(readings);
    }
    if let Some(skipped) = req.readings_skipped {
        active.readings_skipped = Set(skipped);
    }
    if let Some(status_events) = req.status_events_synced {
        active.status_events_synced = Set(status_events);
    }
    if let Some(errors) = req.errors {
        active.errors = Set(Some(errors));
    }
    if let Some(log) = req.log {
        active.log = Set(Some(log));
    }
    if let Some(duration) = req.duration_ms {
        active.duration_ms = Set(Some(duration));
    }
    if is_terminal {
        active.completed_at = Set(Some(Utc::now().into()));
    }

    active.update(&state.db).await?;

    if is_success
        && let Some(service) = services_model::Entity::find_by_id(service_id)
            .one(&state.db)
            .await?
    {
        let mut svc_active: services_model::ActiveModel = service.into();
        svc_active.last_sync_completed_at = Set(Some(Utc::now().into()));
        svc_active.updated_at = Set(Utc::now().into());
        svc_active.update(&state.db).await?;
    }

    Ok(Json(serde_json::json!({"updated": true})))
}
