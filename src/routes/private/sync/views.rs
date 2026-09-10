//! The sync HTTP surface: the control plane a sync service calls, the operator routes a human
//! drives, and the pairing-plan and review-queue handlers.

use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, patch, post};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
};
use chrono::Utc;
use sea_orm::ExprTrait;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, EntityTrait, FromQueryResult,
    QueryFilter, QueryOrder, Set, Statement,
};
use uuid::Uuid;

use river_data_core::commands as core_commands;
use river_data_core::models::{
    CommandStatus, CommandUpdateRequest, EnrollRequest, EnrollResponse, HeartbeatRequest,
    HeartbeatResponse, PendingCommand, ServiceStatus, SyncEventStatus, SyncEventType,
};

use crate::common::AppState;
use crate::common::middleware::{AuthContext, ProjectScope};
use crate::common::paging::{Window, content_range};
use crate::error::{AppError, AppResult};
use crate::routes::private::reprocessing_jobs::reconcile;
use crate::routes::private::sensors;

use super::flows;
use super::models::*;
use super::service::*;

const VALID_UPDATE_STATUSES: &[&str] = &[
    CommandStatus::Acknowledged.as_str(),
    CommandStatus::Completed.as_str(),
    CommandStatus::Failed.as_str(),
];

/// Sync service reports the lifecycle status of a command it received via heartbeat.
/// Valid status transitions: `acknowledged` (in progress), `completed` (success with
/// optional result payload), `failed` (with error result). Only the owning service can
/// update its commands. Requires sync session token auth.
#[utoipa::path(
    patch,
    path = "/api/sync/commands/{id}",
    params(("id" = Uuid, Path, description = "Sync command UUID")),
    request_body = CommandUpdateRequest,
    responses(
        (status = 200, description = "Command updated", body = UpdatedResponse),
        (status = 400, description = "Invalid status value"),
        (status = 401, description = "Invalid session token"),
        (status = 403, description = "Command belongs to a different service"),
        (status = 404, description = "Command not found"),
    ),
    tag = "sync"
)]
pub async fn update_command(
    State(state): State<AppState>,
    ctx: SyncServiceContext,
    Path(command_id): Path<Uuid>,
    Json(req): Json<CommandUpdateRequest>,
) -> AppResult<Json<UpdatedResponse>> {
    let cmd = commands::Entity::find_by_id(command_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Command not found".to_string()))?;

    if cmd.service_id != ctx.service_id {
        return Err(AppError::Forbidden(
            "Command does not belong to this service".to_string(),
        ));
    }

    if !VALID_UPDATE_STATUSES.contains(&req.status.as_str()) {
        return Err(AppError::BadRequest(format!(
            "Invalid status '{}'. Valid: {}",
            req.status,
            VALID_UPDATE_STATUSES.join(", ")
        )));
    }

    let mut active: commands::ActiveModel = cmd.into();
    active.status = Set(req.status.clone());
    if req.result.is_some() {
        active.result = Set(req.result);
    }
    if req.status == CommandStatus::Acknowledged.as_str() {
        active.acknowledged_at = Set(Some(Utc::now().into()));
    }
    if req.status == CommandStatus::Completed.as_str()
        || req.status == CommandStatus::Failed.as_str()
    {
        active.completed_at = Set(Some(Utc::now().into()));
    }
    active.update(&state.db).await?;

    Ok(Json(UpdatedResponse { updated: true }))
}

/// Periodic heartbeat from a sync service. Updates `last_heartbeat`, `status`, and
/// `current_operation`. Returns a session token and any pending commands queued for this service.
/// Requires sync session token auth.
///
/// The token is cached per service for a fraction of its configured lifetime, so a heartbeat
/// usually returns the same token rather than minting one per cycle; rotation happens when that
/// window lapses.
#[utoipa::path(
    post,
    path = "/api/sync/heartbeat",
    request_body = HeartbeatRequest,
    responses(
        (status = 200, description = "Heartbeat acknowledged; fresh token and pending commands", body = HeartbeatResponse),
        (status = 400, description = "Invalid status string"),
        (status = 401, description = "Invalid or expired session token"),
        (status = 403, description = "service_id does not match the authenticated service"),
    ),
    tag = "sync"
)]
pub async fn heartbeat(
    State(state): State<AppState>,
    ctx: SyncServiceContext,
    Json(req): Json<HeartbeatRequest>,
) -> AppResult<Json<HeartbeatResponse>> {
    if req.service_id != ctx.service_id {
        return Err(AppError::Forbidden(
            "Heartbeat service_id does not match the authenticated service".to_string(),
        ));
    }

    if ServiceStatus::from_str(&req.status).is_none() {
        let valid: Vec<&str> = ServiceStatus::ALL.iter().map(|s| s.as_str()).collect();
        return Err(AppError::BadRequest(format!(
            "Invalid status '{}'. Valid: {}",
            req.status,
            valid.join(", ")
        )));
    }

    let service = services::Entity::find_by_id(req.service_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Service not found".to_string()))?;

    let paused = service.paused;
    let sync_interval_secs = service.sync_interval_secs;
    let mut active: services::ActiveModel = service.into();
    active.status = Set(req.status);
    active.current_operation = Set(req.current_operation);
    active.last_heartbeat = Set(Some(Utc::now().into()));
    active.updated_at = Set(Utc::now().into());
    active.update(&state.db).await?;

    let session_token = if let Some(cached) = SESSION_TOKEN_CACHE.get(&req.service_id).await {
        cached
    } else {
        let token = create_session_token(&state, req.service_id).await?;
        SESSION_TOKEN_CACHE
            .insert(req.service_id, token.clone())
            .await;
        token
    };

    let pending = commands::Entity::find()
        .filter(
            Condition::all()
                .add(commands::Column::ServiceId.eq(req.service_id))
                .add(commands::Column::Status.eq(CommandStatus::Pending.as_str()))
                .add(commands::Column::ExpiresAt.gt(Utc::now())),
        )
        .all(&state.db)
        .await?;

    let pending_commands = pending
        .into_iter()
        .map(|c| PendingCommand {
            id: c.id,
            command: c.command,
            payload: c.payload,
        })
        .collect();

    let db_clone = state.db.clone();
    let sid = req.service_id;
    tokio::spawn(async move {
        let _ = commands::Entity::update_many()
            .col_expr(
                commands::Column::Status,
                Expr::value(CommandStatus::Expired.as_str()),
            )
            .filter(commands::Column::ServiceId.eq(sid))
            .filter(commands::Column::Status.eq(CommandStatus::Pending.as_str()))
            .filter(Expr::col(commands::Column::ExpiresAt).lt(Expr::current_timestamp()))
            .exec(&db_clone)
            .await;
    });

    Ok(Json(HeartbeatResponse {
        session_token,
        pending_commands,
        paused,
        sync_interval_secs: sync_interval_secs.map(|s| s as u64),
    }))
}

/// Sync service reports the start of a sync operation. Returns the created event ID
/// (used in subsequent `PATCH /events/{id}` calls). Validates event_type and status
/// against the SyncEventType / SyncEventStatus enums. Requires sync session token auth.
#[utoipa::path(
    post,
    path = "/api/sync/events",
    request_body = CreateSyncEventRequest,
    responses(
        (status = 200, description = "Event created", body = CreatedSyncEventResponse),
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
) -> AppResult<Json<CreatedSyncEventResponse>> {
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

    let event = events::ActiveModel {
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

    Ok(Json(CreatedSyncEventResponse {
        id: inserted.id.to_string(),
        service_id: inserted.service_id,
        status: inserted.status,
    }))
}

/// Sync service updates an in-progress event with metrics, errors, or completion status.
/// Terminal statuses (`completed`/`failed`) auto-stamp `completed_at`. Successful events
/// also update the owning service's `last_sync_completed_at`. Requires sync session token.
#[utoipa::path(
    patch,
    path = "/api/sync/events/{id}",
    params(("id" = Uuid, Path, description = "Sync event UUID")),
    request_body = UpdateSyncEventRequest,
    responses(
        (status = 200, description = "Event updated", body = UpdatedResponse),
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
) -> AppResult<Json<UpdatedResponse>> {
    let event = events::Entity::find_by_id(event_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Sync event not found".to_string()))?;

    if event.service_id != ctx.service_id {
        return Err(AppError::Forbidden(
            "Event does not belong to this service".to_string(),
        ));
    }

    let service_id = event.service_id;
    let mut active: events::ActiveModel = event.into();

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
        && let Some(service) = services::Entity::find_by_id(service_id)
            .one(&state.db)
            .await?
    {
        let mut svc_active: services::ActiveModel = service.into();
        svc_active.last_sync_completed_at = Set(Some(Utc::now().into()));
        svc_active.updated_at = Set(Utc::now().into());
        svc_active.update(&state.db).await?;
    }

    Ok(Json(UpdatedResponse { updated: true }))
}

/// Enroll a sync service instance with credentials. Validates `client_id`/`client_secret`
/// against `credentials_model`, registers or updates a `services_model` row keyed
/// by `(service_type, instance_id)`, and returns a session token used for subsequent
/// authenticated requests (heartbeat, command updates, events). Unauthenticated.
#[utoipa::path(
    post,
    path = "/api/sync/enroll",
    request_body = EnrollRequest,
    responses(
        (status = 200, description = "Service enrolled; session token returned", body = EnrollResponse),
        (status = 401, description = "Invalid client credentials; the reason is not disclosed"),
    ),
    tag = "sync"
)]
pub async fn enroll(
    State(state): State<AppState>,
    Json(req): Json<EnrollRequest>,
) -> AppResult<Json<EnrollResponse>> {
    let found = credentials::Entity::find()
        .filter(credentials::Column::ClientId.eq(&req.client_id))
        .one(&state.db)
        .await?;

    let format = match check_credential(found.as_ref(), &req.client_secret) {
        Ok(format) => format,
        Err(denial) => {
            tracing::warn!(
                client_id = %req.client_id,
                reason = denial.reason(),
                "Enrollment refused"
            );
            return Err(AppError::Unauthorized(denial.public_message().to_string()));
        }
    };
    let cred = found.expect("check_credential admits only a credential that was found");

    // The plaintext is held nowhere, so this enrollment is the only moment a credential stored
    // under the old unsalted digest can be re-hashed without re-issuing it.
    if format == SecretFormat::LegacyDigest {
        let mut upgrade: credentials::ActiveModel = cred.clone().into();
        upgrade.client_secret_hash =
            Set(crate::routes::private::api_tokens::service::hash_api_secret(&req.client_secret));
        if let Err(e) = upgrade.update(&state.db).await {
            tracing::warn!(client_id = %req.client_id, error = %e, "Credential rehash failed");
        } else {
            tracing::info!(client_id = %req.client_id, "Credential rehashed under argon2id");
        }
    }

    let existing = services::Entity::find()
        .filter(
            Condition::all()
                .add(services::Column::ServiceType.eq(&cred.service_type))
                .add(services::Column::InstanceId.eq(&req.instance_id)),
        )
        .one(&state.db)
        .await?;

    let starting = ServiceStatus::Starting.to_string();

    // `paused` deliberately survives re-enrollment: a pod restart must not
    // undo an operator's pause.
    let (service_id, paused, sync_interval_secs) = if let Some(existing) = existing {
        let mut active: services::ActiveModel = existing.clone().into();
        active.status = Set(starting);
        active.current_operation = Set(None);
        active.last_error = Set(None);
        // The credential declares the source system, so a re-declaration reaches the service on
        // its next enrollment rather than waiting for it to be re-created.
        active.source_system = Set(cred.source_system.clone());
        active.updated_at = Set(Utc::now().into());
        active.update(&state.db).await?;
        (existing.id, existing.paused, existing.sync_interval_secs)
    } else {
        let service = services::ActiveModel {
            id: Set(Uuid::new_v4()),
            service_type: Set(cred.service_type.clone()),
            source_system: Set(cred.source_system.clone()),
            instance_id: Set(req.instance_id.clone()),
            status: Set(starting),
            paused: Set(false),
            current_operation: Set(None),
            sync_interval_secs: Set(None),
            full_reassert_enabled: Set(false),
            last_heartbeat: Set(None),
            last_sync_completed_at: Set(None),
            last_error: Set(None),
            created_at: Set(Utc::now().into()),
            updated_at: Set(Utc::now().into()),
        };
        let inserted = service.insert(&state.db).await?;
        (inserted.id, false, None)
    };

    if cred.service_id.is_none() {
        let mut cred_active: credentials::ActiveModel = cred.into();
        cred_active.service_id = Set(Some(service_id));
        cred_active.update(&state.db).await?;
    }

    let session_token = create_session_token(&state, service_id).await?;
    SESSION_TOKEN_CACHE
        .insert(service_id, session_token.clone())
        .await;

    Ok(Json(EnrollResponse {
        service_id,
        session_token,
        paused,
        sync_interval_secs: sync_interval_secs.map(|s| s as u64),
    }))
}

// ============================================================================
// Handlers
// ============================================================================

/// List all registered sync services with their health (computed from `last_heartbeat`
/// age vs the `sync_health_healthy_secs`/`sync_health_warning_secs` thresholds in `Config`).
/// Sorted by `updated_at` DESC. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/services",
    responses(
        (status = 200, description = "Registered sync services with health", body = [SyncServiceResponse]),
    ),
    tag = "sync"
)]
pub async fn list_services(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<SyncServiceResponse>>> {
    let services = services::Entity::find()
        .order_by_desc(services::Column::UpdatedAt)
        .all(&state.db)
        .await?;

    let config = state.config.as_ref();
    let ids: Vec<Uuid> = services.iter().map(|s| s.id).collect();
    let mut errors = recent_errors(&state.db, &ids).await?;
    Ok(Json(
        services
            .into_iter()
            .map(|s| {
                let error = errors.remove(&s.id);
                service_to_response(s, config, error)
            })
            .collect(),
    ))
}

/// Get a single sync service by ID with its computed health. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/services/{id}",
    params(("id" = Uuid, Path, description = "Sync service UUID")),
    responses(
        (status = 200, description = "Sync service detail", body = SyncServiceResponse),
        (status = 404, description = "Service not found"),
    ),
    tag = "sync"
)]
pub async fn get_service(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<SyncServiceResponse>> {
    let service = services::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Service not found".to_string()))?;

    let error = recent_errors(&state.db, &[service.id]).await?.remove(&id);
    Ok(Json(service_to_response(
        service,
        state.config.as_ref(),
        error,
    )))
}

/// Queue a command for a sync service. The command is picked up on the next heartbeat
/// (within `command_expiry_secs`). Valid commands: `trigger_sync`, `trigger_full_sync`,
/// `pause`, `resume`, `resync_streams` with `{"source_keys": [...]}` (re-fetch the named
/// streams from the start of history and ingest with overwrite), and `source_audit` (walk
/// everything the source holds against everything registered here; read-only, its result is the
/// report). Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/sync/services/{id}/commands",
    params(("id" = Uuid, Path, description = "Sync service UUID")),
    request_body = IssueCommandRequest,
    responses(
        (status = 200, description = "Command queued; full command record returned", body = SyncCommandResponse),
        (status = 400, description = "Invalid command name"),
        (status = 404, description = "Service not found"),
    ),
    tag = "sync"
)]
pub async fn issue_command(
    State(state): State<AppState>,
    Path(service_id): Path<Uuid>,
    Json(req): Json<IssueCommandRequest>,
) -> AppResult<Json<SyncCommandResponse>> {
    let service = services::Entity::find_by_id(service_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Service not found".to_string()))?;

    validate_command(&req.command, req.payload.as_ref()).map_err(AppError::BadRequest)?;

    // Persist the desired pause state at issue time so it takes effect even if
    // the service is down and never acknowledges the command.
    let desired_pause = match req.command.as_str() {
        core_commands::PAUSE => Some(true),
        core_commands::RESUME => Some(false),
        _ => None,
    };
    if let Some(paused) = desired_pause {
        let mut active: services::ActiveModel = service.into();
        active.paused = Set(paused);
        active.updated_at = Set(Utc::now().into());
        active.update(&state.db).await?;
    }

    let expiry_secs = state.config.as_ref().sync_command_expiry_secs as i64;
    let cmd = commands::ActiveModel {
        id: Set(Uuid::new_v4()),
        service_id: Set(service_id),
        command: Set(req.command),
        payload: Set(req.payload),
        status: Set(CommandStatus::Pending.to_string()),
        result: Set(None),
        created_at: Set(Utc::now().into()),
        expires_at: Set((Utc::now() + chrono::Duration::seconds(expiry_secs)).into()),
        acknowledged_at: Set(None),
        completed_at: Set(None),
    };

    let inserted = cmd.insert(&state.db).await?;
    Ok(Json(command_to_response(inserted)))
}

/// Update a sync service's operator settings. The service adopts a new cadence on its next
/// heartbeat, with no redeploy and no restart. Requires `write_metadata`.
#[utoipa::path(
    patch,
    path = "/api/sync/services/{id}",
    params(("id" = Uuid, Path, description = "Sync service UUID")),
    request_body = UpdateServiceRequest,
    responses(
        (status = 200, description = "Updated service", body = SyncServiceResponse),
        (status = 400, description = "Cadence below the minimum"),
        (status = 404, description = "Service not found"),
    ),
    tag = "sync"
)]
pub async fn update_service(
    State(state): State<AppState>,
    Path(service_id): Path<Uuid>,
    Json(req): Json<UpdateServiceRequest>,
) -> AppResult<Json<SyncServiceResponse>> {
    let service = services::Entity::find_by_id(service_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Service not found".to_string()))?;

    let last_error = recent_errors(&state.db, &[service.id])
        .await?
        .remove(&service_id);
    if req.sync_interval_secs.is_none() && req.full_reassert_enabled.is_none() {
        return Ok(Json(service_to_response(
            service,
            state.config.as_ref(),
            last_error,
        )));
    }
    if let Some(Some(secs)) = req.sync_interval_secs
        && secs < MIN_SYNC_INTERVAL_SECS
    {
        return Err(AppError::BadRequest(format!(
            "sync_interval_secs must be at least {MIN_SYNC_INTERVAL_SECS} seconds"
        )));
    }

    let mut active: services::ActiveModel = service.into();
    if let Some(interval) = req.sync_interval_secs {
        active.sync_interval_secs = Set(interval);
    }
    if let Some(enabled) = req.full_reassert_enabled {
        active.full_reassert_enabled = Set(enabled);
    }
    active.updated_at = Set(Utc::now().into());
    let updated = active.update(&state.db).await?;
    Ok(Json(service_to_response(
        updated,
        state.config.as_ref(),
        last_error,
    )))
}

/// Paginated list of sync commands (newest first). Returns a `Content-Range: items {start}-{end}/{total}`
/// header for React-admin style pagination. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/commands",
    params(PaginationQuery),
    responses(
        (
            status = 200,
            description = "Page of commands. Response includes a `Content-Range` header with `items start-end/total` for pagination.",
            body = [SyncCommandResponse]
        ),
        (status = 400, description = "per_page is zero"),
    ),
    tag = "sync"
)]
pub async fn list_commands(
    State(state): State<AppState>,
    Query(params): Query<PaginationQuery>,
) -> AppResult<(StatusCode, HeaderMap, Json<Vec<SyncCommandResponse>>)> {
    use sea_orm::PaginatorTrait;

    let window = params.resolve()?;

    let paginator = commands::Entity::find()
        .order_by_desc(commands::Column::CreatedAt)
        .paginate(&state.db, window.limit);

    let total = paginator.num_items().await?;
    let commands: Vec<SyncCommandResponse> = paginator
        .fetch_page(window.page() - 1)
        .await?
        .into_iter()
        .map(command_to_response)
        .collect();

    let headers = content_range(window.offset, commands.len(), total, "items");

    Ok((StatusCode::OK, headers, Json(commands)))
}

/// One command's current state, for polling a command just issued. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/commands/{id}",
    params(("id" = Uuid, Path, description = "Command UUID")),
    responses(
        (status = 200, body = SyncCommandResponse),
        (status = 404, description = "Command not found"),
    ),
    tag = "sync"
)]
pub async fn get_command(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<SyncCommandResponse>> {
    let command = commands::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Command not found".to_string()))?;
    Ok(Json(command_to_response(command)))
}

/// Mint a new enrollment credential (client_id + client_secret). The `client_secret` is
/// returned in plaintext exactly ONCE, only the SHA-256 hash is stored. Used to bootstrap
/// a new sync service instance. Gated by `require_admin` upstream (Keycloak Administrator
/// only, no API token can pass).
#[utoipa::path(
    post,
    path = "/api/sync/credentials",
    request_body = CreateCredentialRequest,
    responses(
        (status = 200, description = "Plaintext client_id and client_secret (only returned once)", body = CreateCredentialResponse),
    ),
    tag = "sync"
)]
pub async fn create_credential(
    State(state): State<AppState>,
    Json(req): Json<CreateCredentialRequest>,
) -> AppResult<Json<CreateCredentialResponse>> {
    let full_token = generate_token();
    let prefix = &state.config.as_ref().sync_client_id_prefix;
    let client_id = format!("{prefix}{}", &full_token[..16]);
    let client_secret = generate_token();
    let secret_hash = crate::routes::private::api_tokens::service::hash_api_secret(&client_secret);

    let cred = credentials::ActiveModel {
        id: Set(Uuid::new_v4()),
        client_id: Set(client_id.clone()),
        client_secret_hash: Set(secret_hash),
        service_type: Set(req.service_type),
        source_system: Set(req
            .source_system
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())),
        service_id: Set(None),
        revoked: Set(false),
        created_at: Set(Utc::now().into()),
    };

    cred.insert(&state.db).await?;

    Ok(Json(CreateCredentialResponse {
        client_id,
        client_secret,
    }))
}

/// List enrollment credentials with their service binding and revocation status. The
/// client_secret is never returned here, only the hash is stored. Gated by `require_admin`
/// upstream, matching credential mint and revoke: a credential bootstraps a full-permission
/// sync session token, so no API token may enumerate them.
#[utoipa::path(
    get,
    path = "/api/sync/credentials",
    responses(
        (status = 200, description = "Credentials list (no secrets)", body = [CredentialResponse]),
    ),
    tag = "sync"
)]
pub async fn list_credentials(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<CredentialResponse>>> {
    let creds = credentials::Entity::find()
        .order_by_desc(credentials::Column::CreatedAt)
        .all(&state.db)
        .await?;

    Ok(Json(
        creds
            .into_iter()
            .map(|c| CredentialResponse {
                id: c.id,
                client_id: c.client_id,
                service_type: c.service_type,
                source_system: c.source_system,
                service_id: c.service_id,
                revoked: c.revoked,
                created_at: c.created_at.to_rfc3339(),
            })
            .collect(),
    ))
}

/// Revoke an enrollment credential and immediately invalidate every active session
/// token bound to its service. Subsequent heartbeat or command updates will be rejected
/// as 401. Requires Keycloak Administrator (`require_admin` upstream).
#[utoipa::path(
    post,
    path = "/api/sync/credentials/{id}/revoke",
    params(("id" = Uuid, Path, description = "Credential UUID")),
    responses(
        (status = 200, description = "Credential revoked, active sessions terminated", body = RevokedResponse),
        (status = 404, description = "Credential not found"),
    ),
    tag = "sync"
)]
pub async fn revoke_credential(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<RevokedResponse>> {
    let cred = credentials::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Credential not found".to_string()))?;

    let mut active: credentials::ActiveModel = cred.clone().into();
    active.revoked = Set(true);
    active.update(&state.db).await?;

    if let Some(service_id) = cred.service_id {
        tokens::Entity::delete_many()
            .filter(tokens::Column::ServiceId.eq(service_id))
            .exec(&state.db)
            .await?;
    }

    Ok(Json(RevokedResponse { revoked: true }))
}

/// Paginated list of sync events (newest first). Returns a `Content-Range` header for
/// React-admin style pagination. Each event records readings/status_events_synced counts,
/// optional errors/log JSON payloads, and duration. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/events",
    params(PaginationQuery),
    responses(
        (
            status = 200,
            description = "Page of sync events. Response includes a `Content-Range` header.",
            body = [SyncEventResponse]
        ),
        (status = 400, description = "per_page is zero"),
    ),
    tag = "sync"
)]
pub async fn list_sync_events(
    State(state): State<AppState>,
    Query(params): Query<PaginationQuery>,
) -> AppResult<(StatusCode, HeaderMap, Json<Vec<SyncEventResponse>>)> {
    use sea_orm::PaginatorTrait;

    let window = params.resolve()?;

    let paginator = events::Entity::find()
        .order_by_desc(events::Column::StartedAt)
        .paginate(&state.db, window.limit);

    let total = paginator.num_items().await?;
    let events = paginator.fetch_page(window.page() - 1).await?;

    let response: Vec<SyncEventResponse> = events.into_iter().map(sync_event_to_response).collect();

    let headers = content_range(window.offset, response.len(), total, "items");

    Ok((StatusCode::OK, headers, Json(response)))
}

/// Revoke a sync service: marks every credential bound to it as revoked AND deletes
/// every active session token. The service is effectively forced off the control plane
/// until a new credential is minted. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/sync/services/{id}/revoke",
    params(("id" = Uuid, Path, description = "Sync service UUID")),
    responses(
        (status = 200, description = "Service revoked", body = RevokedResponse),
    ),
    tag = "sync"
)]
pub async fn revoke_service(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<RevokedResponse>> {
    credentials::Entity::update_many()
        .col_expr(credentials::Column::Revoked, Expr::value(true))
        .filter(credentials::Column::ServiceId.eq(id))
        .exec(&state.db)
        .await?;

    tokens::Entity::delete_many()
        .filter(tokens::Column::ServiceId.eq(id))
        .exec(&state.db)
        .await?;

    Ok(Json(RevokedResponse { revoked: true }))
}

/// The replicate families of a source and their migration state.
#[utoipa::path(
    get,
    path = "/api/sync/replicate_reconciliation/candidates",
    params(("source_system" = String, Query, description = "e.g. cnet")),
    responses((status = 200, body = CandidatesResponse)),
    tag = "sync"
)]
pub async fn reconciliation_candidates(
    State(state): State<AppState>,
    Query(query): Query<CandidatesQuery>,
) -> AppResult<Json<CandidatesResponse>> {
    let pairs = reconcile::family_pairs(&state.db, &query.source_system).await?;
    let mut families = Vec::with_capacity(pairs.len());
    for pair in &pairs {
        let row = state
            .db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT
                     (SELECT COUNT(*)::bigint FROM readings r
                      WHERE r.stream_id = $1 AND r.replicate_index = 0) AS old_readings,
                     (SELECT COUNT(*)::bigint FROM readings o
                      WHERE o.stream_id = $1 AND o.replicate_index = 0
                        AND NOT EXISTS (SELECT 1 FROM readings n
                                        WHERE n.stream_id = $2 AND n.time = o.time)) AS missing",
                [pair.old_id.into(), pair.new_id.into()],
            ))
            .await?
            .ok_or_else(|| AppError::Internal("candidate probe returned no row".to_string()))?;
        let ProbeCounts {
            old_readings,
            missing,
        } = ProbeCounts::from_query_result(&row, "")?;
        families.push(FamilyCandidate {
            family_stream_id: pair.new_id,
            family_source_key: pair.new_key.clone(),
            old_stream_id: pair.old_id,
            old_source_key: pair.old_key.clone(),
            site_parameter_id: pair.old_site_parameter_id,
            migrated: pair.new_paired,
            old_readings,
            missing_instants: missing,
            ready: !pair.new_paired && pair.old_site_parameter_id.is_some() && missing == 0,
        });
    }
    Ok(Json(CandidatesResponse {
        total_old_streams: families.len(),
        families,
    }))
}

/// Start the migrate + verify job. Non-destructive: pairs family streams to their slots and
/// materialises samples; a family failing verification rolls back untouched.
#[utoipa::path(
    post,
    path = "/api/sync/replicate_reconciliation",
    request_body = StartReconciliationRequest,
    responses(
        (status = 200, body = StartReconciliationResponse),
        (status = 409, description = "A reconciliation for this source is already running"),
    ),
    tag = "sync"
)]
pub async fn start_reconciliation(
    State(state): State<AppState>,
    Json(payload): Json<StartReconciliationRequest>,
) -> AppResult<Json<StartReconciliationResponse>> {
    flows::enqueue_reconciliation(&state, "replicate_reconciliation", &payload).await
}

/// Start the delete job: re-verifies each migrated family and removes the obsolete avg streams
/// and their readings. The destructive step of the migration; run only after reviewing the
/// migrate job's verification report.
#[utoipa::path(
    post,
    path = "/api/sync/replicate_reconciliation/delete",
    request_body = StartReconciliationRequest,
    responses(
        (status = 200, body = StartReconciliationResponse),
        (status = 409, description = "A delete for this source is already running"),
    ),
    tag = "sync"
)]
pub async fn start_reconciliation_delete(
    State(state): State<AppState>,
    Json(payload): Json<StartReconciliationRequest>,
) -> AppResult<Json<StartReconciliationResponse>> {
    flows::enqueue_reconciliation(&state, "replicate_reconciliation_delete", &payload).await
}

/// The (site, parameter) slots where two streams carry the same instant. Serving returns one row
/// per instant, so a duplicated slot is invisible on the chart; this is the list the operator
/// reconciles from. Two streams sharing a slot without ever sharing an instant (a sensor feed
/// beside a grab feed) is the normal case and is not listed.
#[utoipa::path(
    get,
    path = "/api/sync/replicate_reconciliation/duplicate_slots",
    responses((status = 200, body = DuplicateSlotsResponse)),
    tag = "sync"
)]
pub async fn duplicate_slots(
    State(state): State<AppState>,
) -> AppResult<Json<DuplicateSlotsResponse>> {
    // Duplication is a property of the pairing: a reading's site and parameter come from its
    // stream's slot, so two streams can only collide at an instant by sharing one.
    let rows = state
        .db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sp.id AS site_parameter_id, sp.site_id, sp.parameter_id, \
                    s.name AS site_name, p.name AS parameter_name, \
                    ds.id AS stream_id, ds.source_system, ds.source_key \
             FROM data_streams ds \
             JOIN site_parameters sp ON sp.id = ds.site_parameter_id \
             JOIN sites s ON s.id = sp.site_id \
             JOIN parameters p ON p.id = sp.parameter_id \
             WHERE sp.id IN ( \
                 SELECT site_parameter_id FROM data_streams \
                 WHERE site_parameter_id IS NOT NULL \
                 GROUP BY site_parameter_id HAVING COUNT(*) > 1 \
             ) \
             ORDER BY s.name, p.name, ds.source_key"
                .to_string(),
        ))
        .await?;

    let mut slots: Vec<DuplicateSlot> = Vec::new();
    for r in rows {
        let slot = ReconciliationSlotRow::from_query_result(&r, "")?;
        let (site_parameter_id, stream_id) = (slot.site_parameter_id, slot.stream_id);
        let stats = state
            .db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT COUNT(*)::bigint AS readings, MIN(time) AS first, MAX(time) AS last \
                 FROM readings WHERE stream_id = $1",
                [stream_id.into()],
            ))
            .await?
            .ok_or_else(|| AppError::Internal("stream probe returned no row".to_string()))?;
        let extent = StreamExtent::from_query_result(&stats, "")?;
        let stream = DuplicateSlotStream {
            stream_id,
            source_system: slot.source_system,
            source_key: slot.source_key,
            readings: extent.readings,
            first_reading: extent.first,
            last_reading: extent.last,
        };
        match slots
            .iter_mut()
            .find(|s| s.site_parameter_id == site_parameter_id)
        {
            Some(slot) => slot.streams.push(stream),
            None => slots.push(DuplicateSlot {
                site_id: slot.site_id,
                site_name: slot.site_name,
                parameter_id: slot.parameter_id,
                parameter_name: slot.parameter_name,
                site_parameter_id,
                streams: vec![stream],
                duplicated_instants: 0,
            }),
        }
    }

    for slot in &mut slots {
        let row = state
            .db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT COUNT(*)::bigint AS c FROM ( \
                     SELECT time FROM readings \
                     WHERE site_id = $1 AND parameter_id = $2 AND withdrawn_at IS NULL \
                     GROUP BY time HAVING COUNT(DISTINCT stream_id) > 1 \
                 ) t",
                [slot.site_id.into(), slot.parameter_id.into()],
            ))
            .await?;
        slot.duplicated_instants = row
            .map(|r| r.try_get::<i64>("", "c"))
            .transpose()?
            .unwrap_or(0);
    }

    // A shared slot is only a duplicate once an instant actually carries both feeds.
    slots.retain(|s| s.duplicated_instants > 0);
    Ok(Json(DuplicateSlotsResponse { slots }))
}

/// List replicate audit holds, newest first. The UI's Audits view reads this.
#[utoipa::path(
    get,
    path = "/api/sync/replicate_audit_holds",
    params(
        ("stream_id" = Option<Uuid>, Query, description = "Filter to one stream"),
        ("stream_ids" = Option<String>, Query, description = "Comma-separated stream UUIDs"),
        ("status" = Option<String>, Query, description = "pending | deferred | acknowledged | remediated | superseded | resolved; omit for pending"),
        ("source_system" = Option<String>, Query, description = "Filter to one source system"),
        ("max_relative_delta" = Option<f64>, Query, description = "Only holds at or below this relative_delta"),
        ("max_mean_relative_delta" = Option<f64>, Query, description = "Only holds at or below this mean_relative_delta"),
        ("max_sd_relative_delta" = Option<f64>, Query, description = "Only holds at or below this sd_relative_delta"),
        ("sort" = Option<String>, Query, description = "relative_delta_desc | relative_delta_asc | created_at_desc"),
        ("page" = Option<u64>, Query, description = "1-based page"),
        ("page_size" = Option<u64>, Query, description = "Default 50, max 500"),
    ),
    responses((status = 200, body = ListHoldsResponse)),
    tag = "sync"
)]
pub async fn list_holds(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(query): Query<ListHoldsQuery>,
) -> AppResult<Json<ListHoldsResponse>> {
    let mut conditions = vec!["TRUE".to_string()];
    let mut binds: Vec<sea_orm::Value> = Vec::new();
    // A restricted caller sees only holds whose stream is paired into their projects; unpaired
    // (deferred) holds belong to no project and are visible only without project restriction.
    if let Some(projects) = scope.sql_project_array() {
        binds.push(projects);
        conditions.push(format!(
            "(EXISTS (SELECT 1 FROM site_parameters sp JOIN sites st ON st.id = sp.site_id \
              WHERE sp.id = ds.site_parameter_id AND st.project_id = ANY(${n})) \
              OR EXISTS (SELECT 1 FROM sites st WHERE st.id = h.site_id \
              AND st.project_id = ANY(${n})))",
            n = binds.len()
        ));
    }
    if let Some(id) = query.id {
        binds.push(id.into());
        conditions.push(format!("h.id = ${}", binds.len()));
    }
    if let Some(stream_id) = query.stream_id {
        binds.push(stream_id.into());
        conditions.push(format!("h.stream_id = ${}", binds.len()));
    }
    if let Some(stream_ids) = query.stream_ids.as_deref().filter(|s| !s.is_empty()) {
        let ids: Vec<Uuid> = stream_ids
            .split(',')
            .map(|s| {
                s.trim()
                    .parse()
                    .map_err(|_| AppError::BadRequest(format!("invalid stream id '{s}'")))
            })
            .collect::<Result<_, _>>()?;
        binds.push(ids.into());
        conditions.push(format!("h.stream_id = ANY(${})", binds.len()));
    }
    if let Some(source_system) = query.source_system.clone() {
        binds.push(source_system.into());
        conditions.push(format!("ds.source_system = ${}", binds.len()));
    }
    if let Some(ceiling) = query.max_relative_delta {
        binds.push(ceiling.into());
        conditions.push(format!("{RELATIVE_DELTA_SQL} <= ${}", binds.len()));
    }
    if let Some(ceiling) = query.max_mean_relative_delta {
        binds.push(ceiling.into());
        conditions.push(format!("{MEAN_RELATIVE_DELTA_SQL} <= ${}", binds.len()));
    }
    if let Some(ceiling) = query.max_sd_relative_delta {
        binds.push(ceiling.into());
        conditions.push(format!("{SD_RELATIVE_DELTA_SQL} <= ${}", binds.len()));
    }
    match query.classification.as_deref() {
        Some("population_sd") => {
            conditions.push(format!(
                "h.kind = 'replicate_stats' AND ({})",
                *POPULATION_SD_SQL
            ));
        }
        Some("not_population_sd") => {
            // COALESCE, not a bare NOT: a hold missing a statistic leaves the signature NULL, and
            // a NULL is not the population signature, so it belongs to the complement. This is
            // what makes the two filters partition the replicate-stats holds exactly, matching
            // the `count(*) FILTER (...)` evidence quoted elsewhere.
            conditions.push(format!(
                "h.kind = 'replicate_stats' AND NOT COALESCE(({}), false)",
                *POPULATION_SD_SQL
            ));
        }
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "classification '{other}' has no filter; only 'population_sd' and \
                 'not_population_sd' are filterable"
            )));
        }
        None => {}
    }
    if let Some(declared) = query.estimator_declared {
        let negate = if declared { "" } else { "NOT " };
        conditions.push(format!(
            "{negate}EXISTS (SELECT 1 FROM site_parameters sp              WHERE sp.id = ds.site_parameter_id AND sp.sd_estimator IS NOT NULL)"
        ));
    }
    // The status view is kept out of the count statement's WHERE so `pending`/`deferred` report
    // the whole backlog under the other filters, whichever view the page shows. Status values
    // come from the allowlist below, so inlining them is safe.
    let base_clause = conditions.join(" AND ");
    let status_sql = match query.status.as_deref() {
        Some(
            s @ ("pending" | "deferred" | "acknowledged" | "remediated" | "superseded"
            | "use_portal" | "use_manual" | "consumed"),
        ) => format!("h.status = '{s}'"),
        Some("resolved") => format!("h.status IN {RESOLVED}"),
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "unknown hold status '{other}'"
            )));
        }
        None => "h.status = 'pending'".to_string(),
    };
    let where_clause = format!("{base_clause} AND {status_sql}");
    let order_by = match query.sort.as_deref() {
        None | Some("created_at_desc") => "h.created_at DESC".to_string(),
        Some("relative_delta_desc") => format!("{RELATIVE_DELTA_SQL} DESC, h.created_at DESC"),
        Some("relative_delta_asc") => format!("{RELATIVE_DELTA_SQL} ASC, h.created_at DESC"),
        Some(other) => {
            return Err(AppError::BadRequest(format!("unknown sort '{other}'")));
        }
    };

    let window = Window::from_page(query.page, query.page_size, 50, 500);
    let (limit, offset) = (window.limit, window.offset);

    let count_row = state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COUNT(*) FILTER (WHERE {status_sql})::bigint AS total,
                        COUNT(*) FILTER (WHERE h.status = 'pending')::bigint AS pending,
                        COUNT(*) FILTER (WHERE h.status = 'deferred')::bigint AS deferred
                 FROM replicate_audit_holds h
                 LEFT JOIN data_streams ds ON ds.id = h.stream_id
                 WHERE {base_clause}"
            ),
            binds.clone(),
        ))
        .await?
        .ok_or_else(|| AppError::Internal("hold count returned no row".to_string()))?;
    let HoldCountsRow {
        total,
        pending,
        deferred,
    } = HoldCountsRow::from_query_result(&count_row, "")?;

    let mut rows = HoldRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT h.id, h.stream_id, h.kind, ds.source_system, ds.source_key, ds.source_name,
                    COALESCE(s.id, es.id) AS site_id,
                    COALESCE(s.name, es.name) AS site_name,
                    COALESCE(p.name, ep.name) AS parameter_name,
                    COALESCE(p.code, ep.code) AS parameter_code,
                    h.tool,
                    COALESCE(ds.site_parameter_id IS NOT NULL, FALSE) AS paired,
                    h.group_time,
                    h.expected, h.computed, h.delta, h.status,
                    ''::text AS classification,
                    CASE WHEN h.kind = 'replicate_stats'
                         THEN COALESCE(h.computed->>'sd_estimator', sp.sd_estimator, 'sample')
                    END AS sd_estimator,
                    h.resolution,
                    h.created_at, h.acknowledged_by, h.acknowledged_at,
                    {RELATIVE_DELTA_SQL} AS relative_delta,
                    {MEAN_RELATIVE_DELTA_SQL} AS mean_relative_delta,
                    {SD_RELATIVE_DELTA_SQL} AS sd_relative_delta
             FROM replicate_audit_holds h
             LEFT JOIN data_streams ds ON ds.id = h.stream_id
             LEFT JOIN site_parameters sp ON sp.id = ds.site_parameter_id
             LEFT JOIN sites s ON s.id = sp.site_id
             LEFT JOIN parameters p ON p.id = sp.parameter_id
             LEFT JOIN sites es ON es.id = h.site_id
             LEFT JOIN parameters ep ON ep.id = h.parameter_id
             WHERE {where_clause}
             ORDER BY {order_by}
             LIMIT {limit} OFFSET {offset}"
        ),
        binds.clone(),
    ))
    .all(&state.db)
    .await?;
    for row in &mut rows {
        // The disagreement signature is a replicate-statistics concept; other kinds carry their
        // meaning in `kind` itself.
        if row.kind == "replicate_stats" {
            row.classification = classify(&row.expected, &row.computed).to_string();
        }
    }

    // Same filters as the counts above, split by kind: the entry points announce what is waiting,
    // and a fired brake is not a replicate-statistics disagreement.
    let kind_rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT h.kind, COUNT(*)::bigint AS n
                 FROM replicate_audit_holds h
                 LEFT JOIN data_streams ds ON ds.id = h.stream_id
                 WHERE {base_clause} AND h.status = 'pending'
                 GROUP BY h.kind"
            ),
            binds.clone(),
        ))
        .await?;
    let mut pending_by_kind = std::collections::BTreeMap::new();
    for row in &kind_rows {
        let row = KindCountRow::from_query_result(row, "")?;
        pending_by_kind.insert(row.kind, u64::try_from(row.n).unwrap_or(0));
    }

    Ok(Json(ListHoldsResponse {
        holds: rows,
        total: u64::try_from(total).unwrap_or(0),
        pending: u64::try_from(pending).unwrap_or(0),
        deferred: u64::try_from(deferred).unwrap_or(0),
        pending_by_kind,
    }))
}

/// Acknowledge one pending hold: the operator confirms the statistics recomputed from the stored
/// replicates. Terminal; re-detection of the same disagreement leaves the decision standing.
/// The acting identity is taken from the caller's authentication, never from the request.
#[utoipa::path(
    post,
    path = "/api/sync/replicate_audit_holds/{id}/acknowledge",
    responses(
        (status = 200, body = AcknowledgeResponse),
        (status = 404, description = "No pending hold with this id"),
    ),
    tag = "sync"
)]
pub async fn acknowledge_hold(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    ProjectScope(scope): ProjectScope,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> AppResult<Json<AcknowledgeResponse>> {
    enforce_hold_scope(&state.db, &scope, id).await?;
    accept_ours(&state, id, &crate::common::actor::label(&auth)).await?;
    Ok(Json(AcknowledgeResponse {
        acknowledged: 1,
        skipped_undeclared_estimator: 0,
    }))
}

/// Resolve one pending hold. Statistics are never written directly: `ours` accepts the
/// recomputed numbers, `flag` marks the named replicates so the trigger recomputes the sample
/// over the rest. Both record the decision on the hold.
#[utoipa::path(
    post,
    path = "/api/sync/replicate_audit_holds/{id}/resolve",
    request_body = ResolveHoldRequest,
    responses(
        (status = 200, body = ResolveHoldResponse),
        (status = 400, description = "Unknown mode, no replicate indexes, an index the hold does \
                                      not name or the group does not hold, an already-flagged \
                                      index, a flag that would leave no unflagged replicate, or \
                                      a legacy hold recorded without indexes"),
        (status = 404, description = "No pending hold with this id"),
    ),
    tag = "sync"
)]
pub async fn resolve_hold(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    ProjectScope(scope): ProjectScope,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Json(payload): Json<ResolveHoldRequest>,
) -> AppResult<Json<ResolveHoldResponse>> {
    enforce_hold_scope(&state.db, &scope, id).await?;
    let by = crate::common::actor::label(&auth);
    match payload.mode.as_str() {
        "ours" => {
            accept_ours(&state, id, &by).await?;
            Ok(Json(ResolveHoldResponse {
                status: "acknowledged".to_string(),
                job_id: None,
                samples_affected: None,
            }))
        }
        "flag" => {
            let mut indexes = payload
                .replicate_indexes
                .clone()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    AppError::BadRequest("flag resolution requires replicate_indexes".to_string())
                })?;
            indexes.sort_unstable();
            indexes.dedup();

            let reason = payload
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map_or_else(|| format!("replicate audit hold {id}"), String::from);
            let index_list = indexes
                .iter()
                .map(i16::to_string)
                .collect::<Vec<_>>()
                .join(", ");

            crate::common::bulk_write::guarded(&state.db, async |txn| {
                // The hold is locked before anything is flagged: the flags and the decision record
                // that explains them must land together or not at all.
                let hold = txn
                    .query_one_raw(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "SELECT stream_id, group_time, resolution, computed
                         FROM replicate_audit_holds
                         WHERE id = $1 AND status = 'pending' FOR UPDATE",
                        [id.into()],
                    ))
                    .await?
                    .ok_or_else(|| {
                        AppError::NotFound(format!("no pending replicate audit hold {id}"))
                    })?;
                let FlagHoldRow {
                    stream_id,
                    group_time,
                    resolution: prev,
                    computed,
                } = FlagHoldRow::from_query_result(&hold, "")?;

                // A flag resolution may only touch the replicates the hold was recorded over: the
                // operator's decision is about those values, and any other index in the group is
                // evidence this hold never showed them.
                let recorded = stored_values(&computed);
                let recorded_indexes: Vec<i16> = recorded.iter().filter_map(|(i, _)| *i).collect();
                if recorded.is_empty() || recorded_indexes.len() != recorded.len() {
                    return Err(AppError::BadRequest(format!(
                        "replicate audit hold {id} predates index recording, so its values cannot \
                         be addressed by replicate index; flag the readings through the readings \
                         flag endpoints instead"
                    )));
                }
                let out_of_hold: Vec<String> = indexes
                    .iter()
                    .filter(|i| !recorded_indexes.contains(i))
                    .map(i16::to_string)
                    .collect();
                if !out_of_hold.is_empty() {
                    return Err(AppError::BadRequest(format!(
                        "replicate index {} is not named by this hold (it records indexes {}); \
                         nothing was flagged",
                        out_of_hold.join(", "),
                        recorded_indexes
                            .iter()
                            .map(i16::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }

                let group_rows = txn
                    .query_all_raw(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "SELECT replicate_index, is_flagged IS TRUE AS flagged FROM readings
                         WHERE stream_id = $1 AND time = $2",
                        [stream_id.into(), group_time.into()],
                    ))
                    .await?;
                let mut existing: Vec<i16> = Vec::with_capacity(group_rows.len());
                let mut already_flagged: Vec<i16> = Vec::new();
                for row in &group_rows {
                    let row = ReplicateStateRow::from_query_result(row, "")?;
                    existing.push(row.replicate_index);
                    if row.flagged {
                        already_flagged.push(row.replicate_index);
                    }
                }
                let absent: Vec<String> = indexes
                    .iter()
                    .filter(|i| !existing.contains(i))
                    .map(i16::to_string)
                    .collect();
                if !absent.is_empty() {
                    return Err(AppError::BadRequest(format!(
                        "no reading at replicate index {} in this group; nothing was flagged",
                        absent.join(", ")
                    )));
                }
                let re_flagged: Vec<String> = indexes
                    .iter()
                    .filter(|i| already_flagged.contains(i))
                    .map(i16::to_string)
                    .collect();
                if !re_flagged.is_empty() {
                    return Err(AppError::BadRequest(format!(
                        "replicate index {} is already flagged; nothing was flagged",
                        re_flagged.join(", ")
                    )));
                }
                // Flagging the whole group would leave the sample trigger with n = 0 and the
                // instant would vanish from serving; a group that bad is retracted at source or
                // through the readings endpoints, not resolved here.
                let survivors = existing
                    .iter()
                    .filter(|i| !already_flagged.contains(i) && !indexes.contains(i))
                    .count();
                if survivors == 0 {
                    return Err(AppError::BadRequest(
                        "at least one unflagged replicate must remain in the group; nothing was \
                         flagged"
                            .to_string(),
                    ));
                }

                // Each flagged replicate is a decision of audit origin (ADR 0008); the record's
                // trigger projects it.
                let flagged = crate::routes::private::readings::decisions::record_many(
                    txn,
                    crate::routes::private::readings::decisions::Kind::Flag,
                    &format!(
                        "r.stream_id = $1 AND r.time = $2 AND r.replicate_index IN ({index_list}) \
                         AND r.is_flagged IS NOT TRUE"
                    ),
                    vec![stream_id.into(), group_time.into()],
                    crate::routes::private::readings::decisions::NewValue::Literal(
                        serde_json::json!({ "reason": reason, "hold_id": id }),
                    ),
                    &by,
                    Some(&reason),
                    crate::routes::private::readings::decisions::Origin::Audit,
                    Some(id),
                )
                .await?;
                if usize::try_from(flagged.rows).unwrap_or(usize::MAX) != indexes.len() {
                    return Err(AppError::Conflict(format!(
                        "the replicate group changed under this request; nothing was flagged \
                         (hold {id})"
                    )));
                }

                let resolution = merged_resolution(
                    prev,
                    serde_json::json!({
                        "action": "flag_replicates",
                        "replicate_indexes": &indexes,
                        "reason": reason,
                    }),
                    &by,
                );
                let decided = txn
                    .execute_raw(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "UPDATE replicate_audit_holds
                         SET status = 'remediated', resolution = $2,
                             acknowledged_by = $3, acknowledged_at = NOW()
                         WHERE id = $1 AND status = 'pending'",
                        [id.into(), resolution.into(), by.clone().into()],
                    ))
                    .await?
                    .rows_affected();
                if decided != 1 {
                    return Err(AppError::Conflict(format!(
                        "replicate audit hold {id} was resolved by another request; nothing was \
                         flagged"
                    )));
                }
                Ok(())
            })
            .await?;
            // The sample trigger recomputed the served statistics for this instant.
            state.response_cache.invalidate_all();
            mint_audit_annotation(
                &state.db,
                id,
                &format!(
                    "Audit remediated: replicate(s) {index_list} flagged, so the mean and sd \
                     recompute over the rest. Reason: {reason}. Flagged by {by}."
                ),
                &by,
            )
            .await;
            Ok(Json(ResolveHoldResponse {
                status: "remediated".to_string(),
                job_id: None,
                samples_affected: None,
            }))
        }
        "estimator" => declare_estimator(&state, id, &payload, &by).await,
        "verify" | "reject" => {
            rule_on_entry(&state, id, &payload.mode, payload.reason.as_deref(), &by).await
        }
        other => Err(AppError::BadRequest(format!(
            "unknown resolve mode '{other}'"
        ))),
    }
}

/// Declare which standard-deviation divisor a slot, or one collection group, publishes.
///
/// This is the resolution the gate points at. It changes a specification, not a statistic: the
/// samples trigger still computes every number from the stored replicates, and all this decides is
/// which of the two divisors it uses. `slot` scope declares it for the parameter at this site and
/// enqueues the retag that brings its existing samples into line; `instant` scope sets it for this
/// one group and leaves the parameter undeclared, so the slot's other holds stay gated.
///
/// Reversible: the previous value is recorded on the resolution, and reopen restores it.
async fn declare_estimator(
    state: &AppState,
    id: Uuid,
    payload: &ResolveHoldRequest,
    by: &str,
) -> AppResult<Json<ResolveHoldResponse>> {
    use crate::routes::private::readings::sd_estimator;

    let estimator = sd_estimator::parse(payload.estimator.as_deref().ok_or_else(|| {
        AppError::BadRequest(
            "an estimator resolution must name 'sample' or 'population'".to_string(),
        )
    })?)?;
    let scope = payload.scope.as_deref().unwrap_or("slot");
    if !matches!(scope, "slot" | "instant") {
        return Err(AppError::BadRequest(format!(
            "unknown estimator scope '{scope}'; expected 'slot' or 'instant'"
        )));
    }

    let (site_parameter_id, affected) =
        crate::common::bulk_write::guarded(&state.db, async |txn| {
            let hold = txn
                .query_one_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT h.group_time, h.resolution, sp.id AS site_parameter_id,
                            sp.site_id, sp.parameter_id, sp.sd_estimator AS previous
                     FROM replicate_audit_holds h
                     JOIN data_streams ds ON ds.id = h.stream_id
                     JOIN site_parameters sp ON sp.id = ds.site_parameter_id
                     WHERE h.id = $1 AND h.status = 'pending'
                     FOR UPDATE OF h",
                    [id.into()],
                ))
                .await?
                .ok_or_else(|| {
                    AppError::NotFound(format!(
                        "no pending replicate audit hold {id} on a paired slot; an estimator is \
                         declared for a slot, so an unpaired stream's hold has none to declare"
                    ))
                })?;
            let EstimatorHoldRow {
                group_time,
                site_parameter_id,
                site_id,
                parameter_id,
                previous,
                resolution: prev_resolution,
            } = EstimatorHoldRow::from_query_result(&hold, "")?;

            let affected = if scope == "slot" {
                set_slot_estimator(txn, site_parameter_id, Some(estimator.to_string())).await?;
                // Counted here, inside the same transaction the declaration lands in, so the
                // number reported is the one the retag will act on.
                txn.query_one_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT COUNT(*)::bigint AS n FROM samples
                     WHERE site_id = $1 AND parameter_id = $2
                       AND sd_estimator IS DISTINCT FROM $3
                       AND sd_estimator_source <> 'sample'",
                    [site_id.into(), parameter_id.into(), estimator.into()],
                ))
                .await?
                .map_or(Ok(0_i64), |row| row.try_get::<i64>("", "n"))?
            } else {
                // One group: set it and refresh that row alone. `sample` as the source is what
                // keeps a later slot-level retag from overwriting this decision.
                let rows = txn
                    .execute_raw(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "UPDATE samples
                         SET sd_estimator = $3, sd_estimator_source = 'sample'
                         WHERE site_id = $1 AND parameter_id = $2 AND collected_at = $4",
                        [
                            site_id.into(),
                            parameter_id.into(),
                            estimator.into(),
                            group_time.into(),
                        ],
                    ))
                    .await?
                    .rows_affected();
                txn.execute_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT refresh_sample_aggregate(id) FROM samples
                     WHERE site_id = $1 AND parameter_id = $2 AND collected_at = $3",
                    [site_id.into(), parameter_id.into(), group_time.into()],
                ))
                .await?;
                i64::try_from(rows).unwrap_or(0)
            };

            let resolution = merged_resolution(
                prev_resolution,
                serde_json::json!({
                    "action": "declare_estimator",
                    "estimator": estimator,
                    "scope": scope,
                    "previous_estimator": previous,
                }),
                by,
            );
            let updated = txn
                .execute_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE replicate_audit_holds
                     SET status = 'remediated', resolution = $2,
                         acknowledged_by = $3, acknowledged_at = NOW()
                     WHERE id = $1 AND status = 'pending'",
                    [id.into(), resolution.into(), by.to_string().into()],
                ))
                .await?
                .rows_affected();
            if updated != 1 {
                return Err(AppError::Conflict(format!(
                    "replicate audit hold {id} was resolved by another request; no estimator was \
                     declared"
                )));
            }
            Ok((site_parameter_id, affected))
        })
        .await?;

    // The slot's existing samples are brought into line by the tracked job, so a long history is
    // visible and rerunnable rather than held open in this request.
    let job_id = if scope == "slot" && affected > 0 {
        crate::routes::private::reprocessing_jobs::worker::enqueue(
            &state.db,
            "sd_estimator_retag",
            None,
            None,
            &serde_json::json!({
                "estimator": estimator,
                "site_parameter_ids": [site_parameter_id],
            }),
            None,
        )
        .await?
    } else {
        None
    };

    let (expected, computed) = hold_numbers(&state.db, id).await;
    let where_ = if scope == "slot" {
        "this parameter"
    } else {
        "this collection group only"
    };
    let divisor = if estimator == "population" {
        "population (divisor n)"
    } else {
        "sample (divisor n-1)"
    };
    mint_audit_annotation(
        &state.db,
        id,
        &format!(
            "Audit resolved by declaration: {where_} publishes its standard deviation with the \
             {divisor} formula ({}). Declared by {by}.",
            disagreement_phrase(&expected, &computed)
        ),
        by,
    )
    .await;
    state.response_cache.invalidate_all();
    Ok(Json(ResolveHoldResponse {
        status: "remediated".to_string(),
        job_id,
        samples_affected: Some(affected),
    }))
}

/// Revert a decision: a remediation's flags are removed (only the readings that resolution
/// flagged, identified by the recorded reason) and the hold returns to review, `pending` or
/// `deferred` per the stream's current pairing.
#[utoipa::path(
    post,
    path = "/api/sync/replicate_audit_holds/{id}/reopen",
    responses(
        (status = 200, body = ResolveHoldResponse),
        (status = 404, description = "No decided hold with this id"),
    ),
    tag = "sync"
)]
pub async fn reopen_hold(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    ProjectScope(scope): ProjectScope,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> AppResult<Json<ResolveHoldResponse>> {
    enforce_hold_scope(&state.db, &scope, id).await?;
    let by = crate::common::actor::label(&auth);
    let reopened = crate::common::bulk_write::guarded(&state.db, async |txn| {
        let hold = txn
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT h.stream_id, h.group_time, h.status, h.resolution,
                        (ds.site_parameter_id IS NOT NULL) AS paired,
                        ds.site_parameter_id, sp.site_id, sp.parameter_id
                 FROM replicate_audit_holds h
                 JOIN data_streams ds ON ds.id = h.stream_id
                 LEFT JOIN site_parameters sp ON sp.id = ds.site_parameter_id
                 WHERE h.id = $1 AND h.status IN ('acknowledged', 'remediated')
                 FOR UPDATE OF h",
                [id.into()],
            ))
            .await?
            .ok_or_else(|| AppError::NotFound(format!("no decided replicate audit hold {id}")))?;
        let ReopenHoldRow {
            stream_id,
            group_time,
            status,
            paired,
            resolution: prev,
            site_parameter_id,
            site_id,
            parameter_id,
        } = ReopenHoldRow::from_query_result(&hold, "")?;

        let flagged = prev.as_ref().and_then(|r| {
            (r.get("action")? == "flag_replicates").then(|| {
                (
                    r.get("replicate_indexes")
                        .and_then(serde_json::Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(serde_json::Value::as_i64)
                                .map(|i| i.to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default(),
                    r.get("reason")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                )
            })
        });

        // An estimator declaration is reverted to exactly what it replaced, which is usually
        // "undeclared" and must go back to NULL rather than to a divisor nobody chose.
        let declared = prev.as_ref().and_then(|r| {
            (r.get("action")? == "declare_estimator").then(|| {
                (
                    r.get("scope")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("slot")
                        .to_string(),
                    r.get("previous_estimator")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    r.get("estimator")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                )
            })
        });

        let reopened = if paired { "pending" } else { "deferred" };
        let resolution = merged_resolution(prev, serde_json::json!({"action": "reopened"}), &by);
        if status == "remediated"
            && let Some((index_list, reason)) = &flagged
            && !index_list.is_empty()
        {
            // Only the rows this resolution flagged: a flag someone set since, or with another
            // reason, stays.
            crate::routes::private::readings::decisions::record_many(
                txn,
                crate::routes::private::readings::decisions::Kind::Unflag,
                &format!(
                    "r.stream_id = $1 AND r.time = $2 AND r.replicate_index IN ({index_list}) \
                     AND r.is_flagged = TRUE AND r.flag_reason = $3"
                ),
                vec![stream_id.into(), group_time.into(), reason.clone().into()],
                crate::routes::private::readings::decisions::NewValue::Literal(
                    serde_json::json!({ "hold_id": id, "reopened": true }),
                ),
                &by,
                Some("reopened"),
                crate::routes::private::readings::decisions::Origin::Audit,
                Some(id),
            )
            .await?;
        }
        if status == "remediated"
            && let Some((decl_scope, previous, _)) = &declared
        {
            if decl_scope == "slot" {
                if let Some(sp_id) = site_parameter_id {
                    set_slot_estimator(txn, sp_id, previous.clone()).await?;
                    // The samples this declaration moved go back with it. A row whose estimator
                    // was chosen for its own instant is not one of them.
                    txn.execute_raw(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "UPDATE samples s
                         SET sd_estimator = COALESCE($3, 'sample'),
                             sd_estimator_source = CASE WHEN $3::text IS NULL
                                                        THEN 'default' ELSE 'slot' END
                         FROM site_parameters sp
                         WHERE sp.id = $1 AND s.site_id = sp.site_id
                           AND s.parameter_id = sp.parameter_id
                           AND s.sd_estimator_source <> 'sample'",
                        [
                            sp_id.into(),
                            previous.clone().into(),
                            previous.clone().into(),
                        ],
                    ))
                    .await?;
                    txn.execute_raw(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "SELECT refresh_sample_aggregate(s.id) FROM samples s
                         JOIN site_parameters sp
                           ON sp.site_id = s.site_id AND sp.parameter_id = s.parameter_id
                         WHERE sp.id = $1 AND s.sd_estimator_source <> 'sample'",
                        [sp_id.into()],
                    ))
                    .await?;
                }
            } else if let (Some(site_id), Some(parameter_id)) = (site_id, parameter_id) {
                // The instant goes back to whatever its slot says, which is the state it would
                // have been created in.
                txn.execute_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE samples s
                     SET sd_estimator = COALESCE(sp.sd_estimator, 'sample'),
                         sd_estimator_source = CASE WHEN sp.sd_estimator IS NULL
                                                    THEN 'default' ELSE 'slot' END
                     FROM site_parameters sp
                     WHERE sp.site_id = s.site_id AND sp.parameter_id = s.parameter_id
                       AND s.site_id = $1 AND s.parameter_id = $2 AND s.collected_at = $3",
                    [site_id.into(), parameter_id.into(), group_time.into()],
                ))
                .await?;
                txn.execute_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT refresh_sample_aggregate(id) FROM samples
                     WHERE site_id = $1 AND parameter_id = $2 AND collected_at = $3",
                    [site_id.into(), parameter_id.into(), group_time.into()],
                ))
                .await?;
            }
        }
        let restored = txn
            .execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE replicate_audit_holds
                 SET status = $2, resolution = $3, acknowledged_by = NULL, acknowledged_at = NULL
                 WHERE id = $1 AND status IN ('acknowledged', 'remediated')",
                [id.into(), reopened.to_string().into(), resolution.into()],
            ))
            .await?
            .rows_affected();
        if restored != 1 {
            return Err(AppError::Conflict(format!(
                "replicate audit hold {id} changed under this request; no flag was reverted"
            )));
        }
        // The note said a decision had been taken here, and it has not any more.
        delete_audit_annotations(txn, id).await?;
        Ok(reopened.to_string())
    })
    .await?;
    state.response_cache.invalidate_all();
    Ok(Json(ResolveHoldResponse {
        status: reopened,
        job_id: None,
        samples_affected: None,
    }))
}

/// Acknowledge pending holds in bulk: one stream or a whole source, optionally bounded by a time
/// window and by a `relative_delta` ceiling, for systematic offsets that would otherwise take one
/// acknowledgement per instant.
#[utoipa::path(
    post,
    path = "/api/sync/replicate_audit_holds/acknowledge_bulk",
    request_body = BulkAcknowledgeRequest,
    responses((status = 200, body = AcknowledgeResponse)),
    tag = "sync"
)]
pub async fn acknowledge_holds_bulk(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Json(payload): Json<BulkAcknowledgeRequest>,
) -> AppResult<Json<AcknowledgeResponse>> {
    let by = crate::common::actor::label(&auth);
    let mut binds: Vec<sea_orm::Value> = vec![by.clone().into()];
    let mut bounds = String::new();
    // A restricted caller acknowledges only holds whose stream is paired to a site in their
    // projects; unpaired (deferred) holds belong to no project and stay out of their reach.
    if let Some(projects) = scope.sql_project_array() {
        binds.push(projects);
        bounds.push_str(&format!(
            " AND EXISTS (SELECT 1 FROM site_parameters sp JOIN sites st ON st.id = sp.site_id \
             WHERE sp.id = ds.site_parameter_id AND st.project_id = ANY(${}))",
            binds.len()
        ));
    }
    if let Some(stream_id) = payload.stream_id {
        binds.push(stream_id.into());
        bounds.push_str(&format!(" AND h.stream_id = ${}", binds.len()));
    }
    if let Some(source_system) = payload.source_system {
        binds.push(source_system.into());
        bounds.push_str(&format!(" AND ds.source_system = ${}", binds.len()));
    }
    if let Some(start) = payload.start {
        binds.push(sea_orm::prelude::DateTimeWithTimeZone::from(start).into());
        bounds.push_str(&format!(" AND h.group_time >= ${}", binds.len()));
    }
    if let Some(end) = payload.end {
        binds.push(sea_orm::prelude::DateTimeWithTimeZone::from(end).into());
        bounds.push_str(&format!(" AND h.group_time <= ${}", binds.len()));
    }
    if let Some(ceiling) = payload.max_relative_delta {
        binds.push(ceiling.into());
        bounds.push_str(&format!(" AND {RELATIVE_DELTA_SQL} <= ${}", binds.len()));
    }
    if let Some(ceiling) = payload.max_mean_relative_delta {
        binds.push(ceiling.into());
        bounds.push_str(&format!(
            " AND {MEAN_RELATIVE_DELTA_SQL} <= ${}",
            binds.len()
        ));
    }
    if let Some(ceiling) = payload.max_sd_relative_delta {
        binds.push(ceiling.into());
        bounds.push_str(&format!(" AND {SD_RELATIVE_DELTA_SQL} <= ${}", binds.len()));
    }
    // The same gate the single acknowledge applies, so a threshold sweep cannot drive around it:
    // at n = 10 the divisor offset is only ~5%, well inside a plausible ceiling.
    let population_sd = &*POPULATION_SD_SQL;
    let undeclared_gate = format!(
        "(({population_sd}) AND h.kind = 'replicate_stats' \
          AND EXISTS (SELECT 1 FROM site_parameters sp \
                      WHERE sp.id = ds.site_parameter_id AND sp.sd_estimator IS NULL))"
    );
    let skipped = state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COUNT(*)::bigint AS n
                 FROM replicate_audit_holds h
                 JOIN data_streams ds ON ds.id = h.stream_id
                 WHERE h.status = 'pending' AND {undeclared_gate}{bounds}"
            ),
            binds.clone(),
        ))
        .await?
        .map_or(Ok(0_i64), |row| row.try_get::<i64>("", "n"))?;

    let resolution_sql = accept_ours_resolution_sql("$1");
    let acknowledged = state
        .db
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "UPDATE replicate_audit_holds AS h
                 SET status = 'acknowledged', resolution = {resolution_sql},
                     acknowledged_by = $1, acknowledged_at = NOW()
                 FROM data_streams ds
                 WHERE ds.id = h.stream_id AND h.status = 'pending'
                   AND NOT {undeclared_gate}{bounds}"
            ),
            binds,
        ))
        .await?
        .rows_affected();
    // One note per instant, as the single acknowledge writes: a sweep is many decisions, and each
    // one is about a value somebody may later look at on a chart. The insert reads the holds this
    // call just decided, identified by the actor and timestamp it stamped on them.
    if acknowledged > 0 {
        let annotated = state
            .db
            .execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "INSERT INTO annotations
                     (site_id, parameter_id, start_time, end_time, text, category,
                      created_by, audit_hold_id)
                 SELECT sp.site_id, sp.parameter_id, h.group_time, h.group_time,
                        'Audit accepted in bulk: the statistics computed here stand (source mean '
                          || COALESCE(round((h.expected->>'mean')::numeric, 4)::text, 'none')
                          || ' sd ' || COALESCE(round((h.expected->>'sd')::numeric, 4)::text, 'none')
                          || ', recomputed mean '
                          || COALESCE(round((h.computed->>'mean')::numeric, 4)::text, 'none')
                          || ' sd ' || COALESCE(round((h.computed->>'sd')::numeric, 4)::text, 'none')
                          || ' over ' || COALESCE(h.computed->>'n', '0')
                          || ' replicates). Accepted by ' || $1 || '.',
                        $2, $1, h.id
                 FROM replicate_audit_holds h
                 JOIN data_streams ds ON ds.id = h.stream_id
                 JOIN site_parameters sp ON sp.id = ds.site_parameter_id
                 WHERE h.status = 'acknowledged' AND h.acknowledged_by = $1
                   AND h.acknowledged_at > NOW() - INTERVAL '1 minute'
                   AND NOT EXISTS (SELECT 1 FROM annotations a WHERE a.audit_hold_id = h.id)",
                [by.into(), AUDIT_ANNOTATION_CATEGORY.into()],
            ))
            .await;
        if let Err(e) = annotated {
            tracing::warn!("could not annotate bulk-acknowledged holds: {e}");
        }
    }
    Ok(Json(AcknowledgeResponse {
        acknowledged,
        skipped_undeclared_estimator: u64::try_from(skipped).unwrap_or(0),
    }))
}

/// Sync admin views are split by required authorization so the unified `/api/` router
/// can layer the right middleware per group without route-level overrides.
///
/// Group membership:
/// - `read_routes`: list/get operations, fine for any read_metadata caller.
/// - `write_routes`: operator actions such as issuing sync commands and pairing workflows.
///   Same gate as other entity mutations (Keycloak admin or write_metadata token).
/// - `manage_routes`: the replicate audit review surface and non-destructive reconciliation.
///   Manager-level humans and above (or a write_metadata token); interns and plain members never
///   see the audit backlog.
/// - `destructive_routes`: the reconciliation delete job, which removes streams and readings, so
///   it takes the same gate as stream deletion on CRUD (Keycloak admin or write_metadata token).
/// - `admin_routes`: credential listing, creation and revoke, these mint full-permission
///   sync session tokens, so they're Keycloak-admin only (no API token can pass). The listing
///   is admin-gated alongside them, matching `sync_service_credentials` CRUD, so a leaked token
///   cannot enumerate which credentials exist.
pub fn read_routes() -> Router<AppState> {
    Router::new()
        .route("/services", get(list_services))
        .route("/services/{id}", get(get_service))
        .route("/commands", get(list_commands))
        .route("/commands/{id}", get(get_command))
        .route("/events", get(list_sync_events))
        .route("/pairing-plans", get(list_pairing_plans))
        .route("/pairing-plans/{id}", get(get_pairing_plan))
        .route("/pairing-plans/{id}/site-metadata", get(plan_site_metadata))
        .route("/pairing-plans/{id}/instruments", get(plan_instruments))
        .route("/unpaired-summary", get(unpaired_summary))
}

pub fn write_routes() -> Router<AppState> {
    Router::new()
        .route("/services/{id}", patch(update_service))
        .route("/services/{id}/commands", post(issue_command))
        .route("/services/{id}/revoke", post(revoke_service))
        .route("/pairing-plans", post(create_pairing_plan))
        .route("/pairing-plans/{id}", patch(update_pairing_plan))
        .route("/pairing-plans/{id}/apply", post(apply_pairing_plan))
        .route(
            "/pairing-plans/{id}/supersede",
            post(supersede_pairing_plan),
        )
        .route("/pairing-plans/{id}/revert", post(revert_pairing_plan))
}

pub fn manage_routes() -> Router<AppState> {
    Router::new()
        .route("/replicate_audit_holds", get(list_holds))
        .route(
            "/replicate_audit_holds/{id}/acknowledge",
            post(acknowledge_hold),
        )
        .route("/replicate_audit_holds/{id}/resolve", post(resolve_hold))
        .route("/replicate_audit_holds/{id}/reopen", post(reopen_hold))
        .route(
            "/replicate_audit_holds/acknowledge_bulk",
            post(acknowledge_holds_bulk),
        )
        .route(
            "/change_proposals",
            get(crate::routes::private::readings::proposals::list_proposals),
        )
        .route(
            "/change_proposals/decide",
            post(crate::routes::private::readings::proposals::decide_proposals),
        )
        .route(
            "/replicate_reconciliation/duplicate_slots",
            get(duplicate_slots),
        )
        .route(
            "/replicate_reconciliation/candidates",
            get(reconciliation_candidates),
        )
        .route("/replicate_reconciliation", post(start_reconciliation))
}

/// Destructive reconciliation: the delete job removes obsolete streams and their readings, so it
/// sits behind the same gate as stream deletion on CRUD (Keycloak Administrator or a
/// write_metadata token), not the manager review layer the non-destructive endpoints use.
pub fn destructive_routes() -> Router<AppState> {
    Router::new().route(
        "/replicate_reconciliation/delete",
        post(start_reconciliation_delete),
    )
}

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/credentials",
            get(list_credentials).post(create_credential),
        )
        .route("/credentials/{id}/revoke", post(revoke_credential))
}

/// Create a draft pairing plan describing a batch of intended stream-to-site_parameter
/// pairings. The plan is reviewable and applied separately. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/sync/pairing-plans",
    request_body(content = Object),
    responses(
        (status = 200, description = "Created pairing plan", body = crate::routes::private::data_streams::pairing_plans::PairingPlan),
    ),
    tag = "sync"
)]
pub async fn create_pairing_plan(
    State(state): State<AppState>,
    Json(req): Json<CreatePairingPlanRequest>,
) -> AppResult<Json<crate::routes::private::data_streams::pairing_plans::PairingPlan>> {
    let plan =
        crate::routes::private::sync::service::create_plan(&state.db, &req.source_system).await?;
    Ok(Json(plan.into()))
}

/// List pairing plans, newest first, optionally narrowed to one source system or one status
/// (draft/applying/applied/reverting/reverted/superseded). Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/pairing-plans",
    params(
        ("source_system" = Option<String>, Query, description = "Only this source system"),
        ("status" = Option<String>, Query, description = "Only this status"),
    ),
    responses(
        (status = 200, description = "Array of pairing plans without their entries", body = Vec<PairingPlanSummary>),
    ),
    tag = "sync"
)]
pub async fn list_pairing_plans(
    State(state): State<AppState>,
    Query(query): Query<ListPairingPlansQuery>,
) -> AppResult<Json<Vec<PairingPlanSummary>>> {
    use crate::routes::private::data_streams::pairing_plans::{Column, Entity};
    use sea_orm::{QueryOrder, QuerySelect};

    let mut select = Entity::find();
    if let Some(source) = query.source_system.as_deref().map(str::trim)
        && !source.is_empty()
    {
        select = select.filter(Column::SourceSystem.eq(source));
    }
    if let Some(status) = query.status.as_deref().map(str::trim)
        && !status.is_empty()
    {
        select = select.filter(Column::Status.eq(status));
    }
    let rows = select
        .select_only()
        .columns([
            Column::Id,
            Column::SourceSystem,
            Column::Status,
            Column::CreatedBy,
            Column::Summary,
            Column::CreatedAt,
            Column::AppliedAt,
        ])
        .order_by_desc(Column::CreatedAt)
        .into_tuple::<(
            Uuid,
            String,
            String,
            Option<String>,
            serde_json::Value,
            chrono::DateTime<chrono::FixedOffset>,
            Option<chrono::DateTime<chrono::FixedOffset>>,
        )>()
        .all(&state.db)
        .await?;

    let mut listing = Vec::with_capacity(rows.len());
    for (id, source_system, status, created_by, summary, created_at, applied_at) in rows {
        let uncovered = if status == "draft" {
            uncovered_stream_count(&state.db, id, &source_system).await?
        } else {
            None
        };
        listing.push(PairingPlanSummary {
            id,
            source_system,
            status,
            created_by,
            summary,
            created_at,
            applied_at,
            uncovered_streams: uncovered,
        });
    }

    Ok(Json(listing))
}

/// Mark a draft superseded, which is what Start over does to the draft it replaces: the decisions
/// stay readable and `apply` refuses it as it refuses an applied plan. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/sync/pairing-plans/{id}/supersede",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    responses(
        (status = 200, description = "Plan superseded", body = PlanStatusChanged),
        (status = 404, description = "Plan not found"),
        (status = 409, description = "Plan not in draft status"),
    ),
    tag = "sync"
)]
pub async fn supersede_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<PlanStatusChanged>> {
    let status = plan_status(&state.db, id).await?;
    if status != "draft" {
        return Err(AppError::Conflict(format!(
            "Plan is '{status}', can only supersede 'draft' plans"
        )));
    }
    claim_plan_status(&state.db, id, "draft", "superseded").await?;
    Ok(Json(PlanStatusChanged {
        id,
        status: "superseded".to_string(),
    }))
}

/// Get a single pairing plan with its full pairing list. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/pairing-plans/{id}",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    responses(
        (status = 200, description = "Pairing plan with intended pairings", body = crate::routes::private::data_streams::pairing_plans::PairingPlan),
        (status = 404, description = "Plan not found"),
    ),
    tag = "sync"
)]
pub async fn get_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<crate::routes::private::data_streams::pairing_plans::PairingPlan>> {
    let plan = crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    Ok(Json(plan.into()))
}

/// Edit a draft pairing plan (only `draft` status allows updates). Requires `write_metadata`.
#[utoipa::path(
    patch,
    path = "/api/sync/pairing-plans/{id}",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    request_body = UpdatePairingPlanRequest,
    responses(
        (status = 200, description = "Updated plan", body = crate::routes::private::data_streams::pairing_plans::PairingPlan),
        (status = 404, description = "Plan not found"),
        (status = 409, description = "Plan not in draft status, or edited since the client read it", body = Object),
    ),
    tag = "sync"
)]
pub async fn update_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(req): Json<UpdatePairingPlanRequest>,
) -> AppResult<Json<crate::routes::private::data_streams::pairing_plans::PairingPlan>> {
    let plan = crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    if plan.status != "draft" {
        return Err(AppError::Conflict(format!(
            "Plan is '{}', can only edit 'draft' plans",
            plan.status
        )));
    }
    if plan.version != req.expected_version {
        return Err(stale_plan(plan.version));
    }

    let mut entries: Vec<crate::routes::private::sync::service::PlanEntry> = plan.entries.0.clone();

    let catalog = crate::routes::private::sync::service::load_entity_catalog(&state.db).await?;

    if let Some(bulk) = &req.bulk {
        if bulk.action != "pair" && bulk.action != "skip" {
            return Err(AppError::BadRequest(format!(
                "Bulk action must be 'pair' or 'skip', got '{}'",
                bulk.action
            )));
        }
        crate::routes::private::sync::service::apply_bulk_action(
            &mut entries,
            &bulk.r#where,
            &bulk.action,
        );
    }

    for update in &req.updates {
        if let Some(entry) = entries.iter_mut().find(|e| e.stream_id == update.stream_id) {
            if let Some(ref action) = update.action {
                entry.action = action.clone();
            }
            if let Some(ref name) = update.project_name {
                entry.project.name = name.clone();
            }
            if let Some(ref name) = update.site_name {
                entry.site.name = name.clone();
            }
            if let Some(ref name) = update.parameter_name {
                entry.parameter.name = name.clone();
            }
            if let Some(ref units) = update.parameter_units {
                entry.parameter.units = units.clone();
            }
            if let Some(ref label) = update.parameter_label {
                entry.parameter.label = Some(label.trim().to_string()).filter(|l| !l.is_empty());
            }
            if let Some(ref declared) = update.sd_estimator {
                // An empty string clears the choice, which is how the review says "leave it
                // undeclared" rather than being unable to take a decision back.
                entry.sd_estimator = if declared.trim().is_empty() {
                    None
                } else {
                    Some(
                        crate::routes::private::readings::sd_estimator::parse(declared)?
                            .to_string(),
                    )
                };
            }
            if let Some(acknowledged) = update.acknowledged {
                entry.acknowledged = acknowledged;
            }
            crate::routes::private::sync::service::reclassify_entry(entry, &catalog);
            // A renamed entry that resolves to an existing site must not carry the stream's
            // coordinates: apply would backfill them onto that unrelated site.
            if update.site_name.is_some() && entry.site.id.is_some() {
                entry.site.latitude = None;
                entry.site.longitude = None;
                entry.site.altitude_m = None;
            }
            if entry.action == "pair"
                && (entry.site.name.trim().is_empty() || entry.parameter.name.trim().is_empty())
            {
                entry.action = "skip".to_string();
                entry
                    .warnings
                    .push(crate::routes::private::sync::service::PlanWarning::empty_name());
            }
        }
    }

    apply_site_attribute_updates(&mut entries, &req.updates);
    apply_instrument_updates(&state, &plan.source_system, &mut entries, &req.updates).await?;

    let mut intents = crate::routes::private::sync::service::plan_curve_intents(&plan)?;
    apply_curve_updates(&state.db, &entries, &mut intents, &req.curves).await?;

    let mut accepted = plan.accepted_objects.0.clone();
    apply_object_updates(
        &mut accepted,
        &req.objects,
        &crate::common::actor::label(&auth),
    );

    let mut proposals = plan.instrument_proposals.0.clone();
    for update in &req.instruments {
        if let Some(proposal) = proposals
            .iter_mut()
            .find(|p| p.source_key == update.source_key)
        {
            proposal.admit = update.admit;
        }
    }

    let summary = serde_json::to_value(crate::routes::private::sync::service::compute_summary_pub(
        &entries,
    ))
    .unwrap_or_default();

    // The write names the version it read, so a second writer who read the same document is
    // refused rather than carrying the first's entries back over the first's decisions.
    let written = state
        .db
        .execute_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE pairing_plans SET entries = $1, curve_assignments = $2, summary = $3, \
             accepted_objects = $4, instrument_proposals = $5, version = version + 1 \
             WHERE id = $6 AND version = $7",
            [
                serde_json::to_value(&entries).unwrap_or_default().into(),
                serde_json::to_value(&intents).unwrap_or_default().into(),
                summary.into(),
                serde_json::to_value(&accepted).unwrap_or_default().into(),
                serde_json::to_value(&proposals).unwrap_or_default().into(),
                id.into(),
                req.expected_version.into(),
            ],
        ))
        .await?;
    if written.rows_affected() == 0 {
        return Err(stale_plan(plan_version(&state.db, id).await?));
    }

    let updated = crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    Ok(Json(updated.into()))
}

/// Apply a pairing plan: execute all its pairings and backfills atomically. Marks the
/// plan as `applied`. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/sync/pairing-plans/{id}/apply",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    request_body(content = Object),
    responses(
        (status = 200, description = "The apply job, to be watched for its counts", body = PlanJobQueued),
        (status = 404, description = "Plan not found"),
        (status = 409, description = "Plan already applied or reverted, or edited since the client read it", body = Object),
    ),
    tag = "sync"
)]
pub async fn apply_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<ApplyPairingPlanRequest>,
) -> AppResult<Json<PlanJobQueued>> {
    // Validate synchronously for immediate feedback, then background the heavy entity-resolution +
    // readings backfill as a tracked job so the request doesn't block. The job's `detail` carries
    // the execution counts the UI used to read from the response.
    let status = plan_status(&state.db, id).await?;
    if status != "draft" {
        return Err(AppError::Conflict(format!(
            "Plan is '{status}', can only apply 'draft' plans"
        )));
    }
    let version = plan_version(&state.db, id).await?;
    if version != req.expected_version {
        return Err(stale_plan(version));
    }
    // Unconfirmed instruments are the operator's decision, so the refusal belongs in the response
    // rather than in a failed job they have to go and read. `apply_plan` checks again: the job is
    // reachable on its own.
    let plan = crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    let entries: Vec<crate::routes::private::sync::service::PlanEntry> = plan.entries.0;
    crate::routes::private::sync::service::refuse_unconfirmed_instruments(&entries)?;

    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        &state.db,
        "plan_apply",
        None,
        Some(id),
        &serde_json::json!({ "plan_id": id }),
        None,
    )
    .await?;
    Ok(Json(PlanJobQueued {
        job_id,
        status: "queued".to_string(),
    }))
}

/// Revert an applied pairing plan: unpair every stream it touched, restoring the prior
/// state. Marks the plan as `reverted`. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/sync/pairing-plans/{id}/revert",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    responses(
        (status = 200, description = "The revert job, to be watched for its counts", body = PlanJobQueued),
        (status = 404, description = "Plan not found"),
        (status = 409, description = "Plan not in applied status"),
    ),
    tag = "sync"
)]
pub async fn revert_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<PlanJobQueued>> {
    let status = plan_status(&state.db, id).await?;
    if status != "applied" {
        return Err(AppError::Conflict(format!(
            "Plan is '{status}', can only revert 'applied' plans"
        )));
    }
    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        &state.db,
        "plan_revert",
        None,
        Some(id),
        &serde_json::json!({ "plan_id": id }),
        None,
    )
    .await?;
    Ok(Json(PlanJobQueued {
        job_id,
        status: "queued".to_string(),
    }))
}

/// Aggregate summary of unpaired streams grouped by source system. Used by the dashboard
/// to surface streams needing attention. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/unpaired-summary",
    responses(
        (status = 200, description = "Counts of unpaired streams by source_system", body = Vec<UnpairedSummaryRow>),
    ),
    tag = "sync"
)]
pub async fn unpaired_summary(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<UnpairedSummaryRow>>> {
    use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
    let rows = state
        .db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT source_system, \
                COUNT(*) FILTER (WHERE site_parameter_id IS NULL) as unpaired, \
                COUNT(*) FILTER (WHERE site_parameter_id IS NOT NULL) as paired \
         FROM data_streams GROUP BY source_system ORDER BY source_system"
                .to_owned(),
        ))
        .await?;

    let result = rows
        .iter()
        .map(|row| UnpairedSummaryRow::from_query_result(row, ""))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Json(result))
}

/// Get site metadata enrichment for a pairing plan: latitudes, longitudes, glacier names,
/// stream counts. Used by the pairing UI to display context. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/pairing-plans/{id}/site-metadata",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    responses(
        (status = 200, description = "One row per site the plan covers", body = Vec<PlanSiteMetadata>),
        (status = 404, description = "Plan not found"),
    ),
    tag = "sync"
)]
pub async fn plan_site_metadata(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<PlanSiteMetadata>>> {
    use sea_orm::{FromQueryResult, Statement};

    let plan = crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    let entries: Vec<crate::routes::private::sync::service::PlanEntry> = plan.entries.0.clone();

    let stream_ids: Vec<Uuid> = entries.iter().map(|e| e.stream_id).collect();
    if stream_ids.is_empty() {
        return Ok(Json(vec![]));
    }

    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT DISTINCT ON (metadata->'hierarchy'->>'site') \
            metadata->'hierarchy'->>'site' as site_name, \
            metadata->'coordinates'->>'latitude' as latitude, \
            metadata->'coordinates'->>'longitude' as longitude, \
            metadata->'coordinates'->>'altitude_m' as altitude_m, \
            metadata->'glacier'->>'name' as glacier_name, \
            metadata->'glacier'->>'rgi_v6' as glacier_rgi, \
            metadata->'location'->>'type' as location_type, \
            metadata->'station'->>'catchment' as catchment, \
            metadata->'station'->>'full_name' as full_name, \
            metadata->'station'->>'elevation' as elevation, \
            metadata->>'channel_id' as channel_id, \
            metadata->>'sample_interval_sec' as sample_interval_sec \
         FROM data_streams WHERE id = ANY($1) \
         ORDER BY metadata->'hierarchy'->>'site'",
            [sea_orm::Value::Array(
                sea_orm::sea_query::ArrayType::Uuid,
                Some(Box::new(
                    stream_ids
                        .iter()
                        .map(|id| sea_orm::Value::Uuid(Some(*id)))
                        .collect(),
                )),
            )],
        ))
        .await?;

    // A JSON field that was never written reads as absent, and one written as the string "null"
    // or as empty says the source had nothing there, which is the same thing.
    fn present(value: Option<String>) -> Option<String> {
        value.filter(|s| s != "null" && !s.is_empty())
    }
    fn number(value: Option<String>) -> Option<f64> {
        present(value).and_then(|s| s.parse::<f64>().ok())
    }

    let mut result: Vec<PlanSiteMetadata> = rows
        .iter()
        .map(|row| {
            let r = PlanSiteMetadataRow::from_query_result(row, "")?;
            Ok(PlanSiteMetadata {
                site_name: r.site_name.unwrap_or_default(),
                latitude: number(r.latitude),
                longitude: number(r.longitude),
                altitude_m: number(r.altitude_m),
                glacier_name: present(r.glacier_name),
                glacier_rgi: present(r.glacier_rgi),
                location_type: present(r.location_type),
                catchment: present(r.catchment),
                full_name: present(r.full_name),
                elevation: number(r.elevation),
                channel_id: present(r.channel_id),
                sample_interval_sec: present(r.sample_interval_sec)
                    .and_then(|s| s.parse::<i64>().ok()),
                devices: Vec::new(),
            })
        })
        .collect::<Result<Vec<_>, sea_orm::DbErr>>()?;

    // Devices are counted per site, not folded into the site row: a site instrumented with two
    // loggers has two, and reporting one of them names channels that belong to the other.
    let device_rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT metadata->'hierarchy'->>'site' AS site_name, \
                    metadata->'device'->>'logger_serial' AS serial, \
                    max(metadata->'device'->>'logger_device') AS model, \
                    count(*) AS streams \
             FROM data_streams \
             WHERE id = ANY($1) AND metadata->'device'->>'logger_serial' <> '' \
             GROUP BY 1, 2 ORDER BY 1, 2",
            [sea_orm::Value::Array(
                sea_orm::sea_query::ArrayType::Uuid,
                Some(Box::new(
                    stream_ids
                        .iter()
                        .map(|id| sea_orm::Value::Uuid(Some(*id)))
                        .collect(),
                )),
            )],
        ))
        .await?;
    let mut devices_by_site: std::collections::HashMap<String, Vec<PlanSiteDevice>> =
        std::collections::HashMap::new();
    for row in &device_rows {
        let r = PlanSiteDeviceRow::from_query_result(row, "")?;
        let Some(serial) = r.serial.filter(|s| !s.is_empty()) else {
            continue;
        };
        devices_by_site
            .entry(r.site_name.unwrap_or_default())
            .or_default()
            .push(PlanSiteDevice {
                serial,
                model: r.model,
                streams: r.streams,
            });
    }

    for site in &mut result {
        site.devices = devices_by_site.remove(&site.site_name).unwrap_or_default();
    }

    Ok(Json(result))
}

/// The instrument picture of a pairing plan: every instrument the plan binds, and the parameters
/// still without one. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/pairing-plans/{id}/instruments",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    responses(
        (status = 200, description = "The plan's instruments and unassigned parameters", body = PlanInstrumentsResponse),
        (status = 404, description = "Plan not found"),
    ),
    tag = "sync"
)]
pub async fn plan_instruments(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<PlanInstrumentsResponse>> {
    let plan = crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    let entries: Vec<crate::routes::private::sync::service::PlanEntry> = plan.entries.0.clone();
    // Read fresh rather than from the stored plan: an instrument created since the plan was drafted
    // is exactly the collision this reports.
    let catalog = crate::routes::private::sync::service::load_instrument_catalog(
        &state.db,
        &plan.source_system,
        &[],
    )
    .await?;

    struct Acc {
        instrument: crate::routes::private::sync::service::PlanInstrumentRef,
        anchor: Uuid,
        streams: usize,
        parameters: std::collections::BTreeSet<String>,
        sites: std::collections::BTreeSet<String>,
    }
    let mut bound: std::collections::BTreeMap<String, Acc> = std::collections::BTreeMap::new();
    let mut unassigned: std::collections::BTreeMap<String, PlanUnassignedParameter> =
        std::collections::BTreeMap::new();

    for entry in entries.iter().filter(|e| e.action == "pair") {
        let scope = instrument_key(entry);
        // A device-shaped feed is reported as its device, whether or not it already names one.
        // Listing it as a lab decision as well would put the same instrument in two places, one of
        // which offers to change it.
        if entry.is_device {
            continue;
        }
        match &entry.instrument {
            Some(instrument) => {
                let acc = bound.entry(scope).or_insert_with(|| Acc {
                    instrument: instrument.clone(),
                    anchor: entry.stream_id,
                    streams: 0,
                    parameters: std::collections::BTreeSet::new(),
                    sites: std::collections::BTreeSet::new(),
                });
                acc.streams += 1;
                acc.parameters.insert(entry.parameter.name.clone());
                acc.sites.insert(entry.site.name.clone());
            }
            None => {
                let e =
                    unassigned
                        .entry(scope.clone())
                        .or_insert_with(|| PlanUnassignedParameter {
                            scope,
                            // The parameter alone: see `resolve_parameter_instrument`. The two
                            // producers spelled the suffix differently, which made one row read
                            // as two instruments.
                            suggested_name: entry.parameter.name.clone(),
                            name_conflict: catalog.named(&entry.parameter.name),
                            parameter: entry.parameter.name.clone(),
                            stream_count: 0,
                            site_count: 0,
                            anchor_stream_id: entry.stream_id,
                        });
                e.stream_count += 1;
            }
        }
    }
    // Site breadth per unassigned parameter, counted the same way as a bound group's.
    let mut unassigned_sites: std::collections::BTreeMap<
        String,
        std::collections::BTreeSet<String>,
    > = std::collections::BTreeMap::new();
    for entry in entries.iter().filter(|e| e.action == "pair") {
        if entry.instrument.is_none() && !entry.is_device {
            unassigned_sites
                .entry(instrument_scope(entry))
                .or_default()
                .insert(entry.site.name.clone());
        }
    }
    for (scope, sites) in unassigned_sites {
        if let Some(u) = unassigned.get_mut(&scope) {
            u.site_count = sites.len();
        }
    }

    let mut groups: Vec<PlanInstrumentGroup> = bound
        .into_iter()
        .map(|(scope, acc)| PlanInstrumentGroup {
            scope: Some(scope),
            instrument_id: acc.instrument.id,
            name: acc.instrument.name,
            source_key: acc.instrument.source_key,
            resolved_by: acc.instrument.resolved_by,
            create: acc.instrument.create,
            confirmed: acc.instrument.confirmed,
            stamps_readings: acc.instrument.stamps_readings,
            curve_column: acc.instrument.curve_column,
            stream_count: acc.streams,
            parameters: acc.parameters.into_iter().collect(),
            site_count: acc.sites.len(),
            anchor_stream_id: Some(acc.anchor),
            proposed_name: acc.instrument.proposed_name,
            name_conflict: acc.instrument.name_conflict,
            curves: acc.instrument.curves,
        })
        .collect();

    // Anything still asking first, then by breadth: the decisions come before the inventory.
    groups.sort_by(|a, b| {
        (
            a.confirmed,
            std::cmp::Reverse(a.stream_count),
            a.name.clone(),
        )
            .cmp(&(
                b.confirmed,
                std::cmp::Reverse(b.stream_count),
                b.name.clone(),
            ))
    });

    // Every curve the source replicated, with the instrument it sits on and how much data it has
    // corrected. Independent of what this plan binds: a curve on the wrong instrument is a thing to
    // fix whether or not a stream in this plan names it.
    let instrument_names: std::collections::HashMap<Uuid, String> = sensors::Entity::find()
        .filter(sensors::Column::SourceSystem.eq(plan.source_system.clone()))
        .all(&state.db)
        .await?
        .into_iter()
        .map(|s| {
            let name = s
                .name
                .clone()
                .or_else(|| s.serial_number.clone())
                .unwrap_or_else(|| s.id.to_string());
            (s.id, name)
        })
        .collect();
    let mut usage: std::collections::HashMap<Uuid, i64> = std::collections::HashMap::new();
    for row in state
        .db
        .query_all_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT standard_curve_id AS id, COUNT(*) AS n FROM readings
             WHERE standard_curve_id IS NOT NULL GROUP BY standard_curve_id"
                .to_string(),
        ))
        .await?
    {
        usage.insert(row.try_get::<Uuid>("", "id")?, row.try_get::<i64>("", "n")?);
    }
    let intents = crate::routes::private::sync::service::plan_curve_intents(&plan)?;
    let proposed_names: std::collections::HashMap<&str, &str> = entries
        .iter()
        .filter_map(|e| e.instrument.as_ref())
        .filter(|i| i.create)
        .map(|i| (i.source_key.as_str(), i.name.as_str()))
        .collect();
    let mut curves: Vec<PlanCurveAssignment> =
        crate::routes::private::sensors::standard_curves::Entity::find()
            .filter(
                crate::routes::private::sensors::standard_curves::Column::SourceSystem
                    .eq(plan.source_system.clone()),
            )
            .all(&state.db)
            .await?
            .into_iter()
            .map(|c| {
                let pending = intents.iter().find(|i| i.curve_id == c.id);
                PlanCurveAssignment {
                    instrument_name: instrument_names
                        .get(&c.sensor_id)
                        .cloned()
                        .unwrap_or_else(|| c.sensor_id.to_string()),
                    reading_count: usage.get(&c.id).copied().unwrap_or(0),
                    pending_source_key: pending.map(|i| i.instrument_source_key.clone()),
                    pending_instrument_name: pending.and_then(|i| {
                        proposed_names
                            .get(i.instrument_source_key.as_str())
                            .map(|n| (*n).to_string())
                    }),
                    id: c.id,
                    name: c.name,
                    slope: c.slope,
                    intercept: c.intercept,
                    r_squared: c.r_squared,
                    source_key: c.source_key,
                    sensor_id: c.sensor_id,
                }
            })
            .collect();
    curves.sort_by(|a, b| {
        a.instrument_name
            .cmp(&b.instrument_name)
            .then_with(|| a.name.cmp(&b.name))
    });

    // The devices the plan's feeds name, one row per channel: a channel is an instrument, so the
    // key is the feed's own `source_key`. Grouping by serial would merge a multi-channel logger's
    // parameters into one row and then fail to resolve it, since a source-registered instrument
    // carries no serial.
    let mut device_acc: std::collections::BTreeMap<(String, String), PlanDeviceGroup> =
        std::collections::BTreeMap::new();
    for entry in entries.iter().filter(|e| e.action == "pair") {
        if !entry.is_device {
            continue;
        }
        let group = device_acc
            .entry((entry.site.name.clone(), entry.source_key.clone()))
            .or_insert_with(|| PlanDeviceGroup {
                site: entry.site.name.clone(),
                serial: entry.device_serial.clone().unwrap_or_default(),
                model: entry.device_model.clone(),
                instrument_id: None,
                instrument_name: None,
                parameters: Vec::new(),
                stream_count: 0,
                anchor_stream_id: entry.stream_id,
            });
        group.stream_count += 1;
        if !group.parameters.contains(&entry.parameter.name) {
            group.parameters.push(entry.parameter.name.clone());
        }
    }
    if !device_acc.is_empty() {
        let keys: Vec<String> = device_acc.keys().map(|(_, k)| k.clone()).collect();
        for sensor in sensors::Entity::find()
            .filter(sensors::Column::SourceSystem.eq(plan.source_system.as_str()))
            .filter(sensors::Column::SourceKey.is_in(keys))
            .all(&state.db)
            .await?
        {
            let Some(source_key) = sensor.source_key.clone() else {
                continue;
            };
            for group in device_acc
                .iter_mut()
                .filter(|((_, k), _)| *k == source_key)
                .map(|(_, g)| g)
            {
                group.instrument_id = Some(sensor.id);
                group.instrument_name = sensor.name.clone().or_else(|| sensor.source_key.clone());
            }
        }
    }

    Ok(Json(PlanInstrumentsResponse {
        groups,
        unassigned: unassigned.into_values().collect(),
        devices: device_acc.into_values().collect(),
        curves,
    }))
}

/// The credential-authenticated entry point, the only route worth brute-forcing.
pub fn control_enroll_routes() -> Router<AppState> {
    Router::new().route("/enroll", post(enroll))
}

/// Session-token routes. Deliberately unthrottled: the callers are vetted internal services and
/// the events route is their observability record — a 429 here once lost METALP's cycle record
/// while its data synced fully.
pub fn control_session_routes() -> Router<AppState> {
    Router::new()
        .route("/heartbeat", post(heartbeat))
        .route("/commands/{id}", patch(update_command))
        .route("/events", post(create_sync_event))
        .route("/events/{id}", patch(update_sync_event))
}

pub fn control_routes() -> Router<AppState> {
    control_enroll_routes().merge(control_session_routes())
}

#[cfg(test)]
#[path = "tests/views.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/operator.rs"]
mod operator_tests;
