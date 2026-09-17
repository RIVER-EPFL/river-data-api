//! The sync HTTP surface: the control plane a sync service calls, the operator routes a human
//! drives, and the pairing-plan and review-queue handlers.

use axum::middleware;
use axum::routing::{get, patch, post};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
};
use chrono::Utc;
use sea_orm::ExprTrait;
use sea_orm::sea_query::{
    Alias, Expr, JoinType, LockType, PostgresQueryBuilder, Query as SeaQuery, SelectStatement,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, EntityTrait, FromQueryResult,
    QueryFilter, Set, Statement,
};
use tower_http::limit::RequestBodyLimitLayer;
use uuid::Uuid;

use crate::common::middleware::{
    deny_scoped_token, require_admin, require_admin_or_token_write_metadata,
    require_manage_sensors, require_read_metadata,
};
use crate::routes::service::ACTION_BODY_LIMIT;
use river_data_core::commands as core_commands;

use crate::routes::private::annotations::models as annotations;
use crate::routes::private::data_streams::models as data_streams;
use crate::routes::private::readings::models as readings;
use crate::routes::private::site_parameters::models as site_parameters;
use river_data_core::models::{
    CommandStatus, CommandUpdateRequest, EnrollRequest, EnrollResponse, HeartbeatRequest,
    HeartbeatResponse, PendingCommand, ServiceStatus, SyncEventStatus, SyncEventType,
};

use crate::common::AppState;
use crate::common::middleware::{AuthContext, ProjectScope};
use crate::common::paging::Window;
use crate::error::{AppError, AppResult};
use crate::routes::private::sensors;
use crate::routes::private::sync::hold_model;

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

    if ServiceStatus::parse(&req.status).is_none() {
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
        && SyncEventType::parse(event_type).is_none()
    {
        let valid: Vec<&str> = SyncEventType::ALL.iter().map(|v| v.as_str()).collect();
        return Err(AppError::BadRequest(format!(
            "Invalid event_type '{}'. Valid: {}",
            event_type,
            valid.join(", ")
        )));
    }

    if let Some(ref status) = req.status
        && SyncEventStatus::parse(status).is_none()
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

    let parsed_status = req.status.as_deref().and_then(SyncEventStatus::parse);
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
        active.updated_at = Set(Utc::now().into());
        active.update(&state.db).await?;
        (existing.id, existing.paused, existing.sync_interval_secs)
    } else {
        let service = services::ActiveModel {
            id: Set(Uuid::new_v4()),
            service_type: Set(cred.service_type.clone()),
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

/// The three table aliases every hold statement here reads through, so a column reference names
/// the same table in each of them.
const HOLD: &str = "h";
const STREAM: &str = "ds";
const SLOT: &str = "sp";

/// A hold joined to the stream that raised it, and that stream's slot. The join to the slot is the
/// caller's: an inner join asks for a hold on a paired stream, a left join admits an unpaired one.
fn hold_on_its_stream(to_slot: JoinType) -> SelectStatement {
    SeaQuery::select()
        .from_as(hold_model::Entity, Alias::new(HOLD))
        .join_as(
            JoinType::InnerJoin,
            data_streams::Entity,
            Alias::new(STREAM),
            Expr::col((Alias::new(STREAM), data_streams::Column::Id))
                .equals((Alias::new(HOLD), hold_model::Column::StreamId)),
        )
        .join_as(
            to_slot,
            site_parameters::Entity,
            Alias::new(SLOT),
            Expr::col((Alias::new(SLOT), site_parameters::Column::Id))
                .equals((Alias::new(STREAM), data_streams::Column::SiteParameterId)),
        )
        .take()
}

/// A hold whose stream is paired, read through that pairing.
fn hold_on_its_slot() -> SelectStatement {
    hold_on_its_stream(JoinType::InnerJoin)
}

/// The holds this bulk acknowledge has not already put a note on. An acknowledge that ran twice
/// inside the minute the statement looks back over would otherwise annotate the same instant
/// again.
fn not_yet_annotated() -> SelectStatement {
    let existing = Alias::new("a");
    SeaQuery::select()
        .expr(Expr::val(1))
        .from_as(annotations::Entity, existing.clone())
        .and_where(
            Expr::col((existing, annotations::Column::AuditHoldId))
                .equals((Alias::new(HOLD), hold_model::Column::Id)),
        )
        .take()
}

/// One built statement, ready to execute. Select, update or insert: every statement here reaches
/// the connection this way, so nothing in the file hands the driver SQL it assembled itself.
fn built<Q: sea_orm::sea_query::QueryStatementWriter>(query: Q) -> Statement {
    let (sql, values) = query.build(PostgresQueryBuilder);
    Statement::from_sql_and_values(sea_orm::DatabaseBackend::Postgres, sql, values)
}

/// List replicate audit holds, newest first. The UI's Audits view reads this.
#[utoipa::path(
    get,
    path = "/api/sync/replicate_audit_holds",
    params(
        ("stream_id" = Option<Uuid>, Query, description = "Filter to one stream"),
        ("stream_ids" = Option<String>, Query, description = "Comma-separated stream UUIDs"),
        ("status" = Option<String>, Query, description = "pending | deferred | acknowledged | remediated | superseded | resolved | any; omit for pending"),
        ("source_system" = Option<String>, Query, description = "Filter to one source system"),
        ("max_relative_delta" = Option<f64>, Query, description = "Only holds at or below this relative_delta"),
        ("max_mean_relative_delta" = Option<f64>, Query, description = "Only holds at or below this mean_relative_delta"),
        ("max_sd_relative_delta" = Option<f64>, Query, description = "Only holds at or below this sd_relative_delta"),
        ("sort" = Option<String>, Query, description = "relative_delta_desc | relative_delta_asc | created_at_desc"),
        ("kind" = Option<String>, Query, description = "Comma-separated hold kinds"),
        ("tool" = Option<String>, Query, description = "Filter to holds raised against one calculation"),
        ("site_id" = Option<Uuid>, Query, description = "Holds at one site, through the pairing or the finding"),
        ("parameter_id" = Option<Uuid>, Query, description = "Holds on one parameter, through the pairing or the finding"),
        ("from" = Option<DateTime<Utc>>, Query, description = "Holds whose instant is at or after this"),
        ("to" = Option<DateTime<Utc>>, Query, description = "Holds whose instant is before this"),
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
    let filters = hold_filters(&scope, &query)?;
    let status = hold_status_condition(query.status.as_deref())?;
    let order_by = hold_order_by(query.sort.as_deref())?;
    let window = Window::from_page(query.page, query.page_size, 50, 500);

    let count_row = state
        .db
        .query_one_raw(built(hold_counts_statement(&filters, &status)))
        .await?
        .ok_or_else(|| AppError::Internal("hold count returned no row".to_string()))?;
    let HoldCountsRow {
        total,
        pending,
        deferred,
    } = HoldCountsRow::from_query_result(&count_row, "")?;

    let mut rows = HoldRow::find_by_statement(built(hold_list_statement(
        &filters,
        &status,
        &order_by,
        window.limit,
        window.offset,
    )))
    .all(&state.db)
    .await?;
    for row in &mut rows {
        // The disagreement signature is a replicate-statistics concept; other kinds carry their
        // meaning in `kind` itself.
        if row.kind == HoldKind::ReplicateStats.as_str() {
            row.classification = classify(&row.expected, &row.computed).to_string();
        }
    }

    let kind_rows = state
        .db
        .query_all_raw(built(hold_kind_counts_statement(&filters)))
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

/// Move a hold to a terminal status, but only from the statuses that transition takes it from.
/// The count returned is what tells a caller its decision landed rather than raced another.
async fn decide_hold<C: sea_orm::ConnectionTrait>(
    conn: &C,
    id: Uuid,
    from: &[HoldStatus],
    to: HoldStatus,
    resolution: serde_json::Value,
    by: Option<&str>,
) -> AppResult<u64> {
    use super::hold_model::{Column, Entity};
    let acknowledged_at = match by {
        Some(_) => sea_orm::sea_query::Expr::current_timestamp(),
        None => {
            sea_orm::sea_query::Expr::value(Option::<sea_orm::prelude::DateTimeWithTimeZone>::None)
        }
    };
    let result = Entity::update_many()
        .col_expr(Column::Status, sea_orm::sea_query::Expr::value(to.as_str()))
        .col_expr(
            Column::Resolution,
            sea_orm::sea_query::Expr::value(resolution),
        )
        .col_expr(
            Column::AcknowledgedBy,
            sea_orm::sea_query::Expr::value(by.map(ToString::to_string)),
        )
        .col_expr(Column::AcknowledgedAt, acknowledged_at)
        .filter(Column::Id.eq(id))
        .filter(Column::Status.is_in(from.iter().map(|s| s.as_str())))
        .exec(conn)
        .await?;
    Ok(result.rows_affected)
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
        skipped_no_stream: 0,
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
                    .query_one_raw(built(
                        SeaQuery::select()
                            .columns([
                                hold_model::Column::StreamId,
                                hold_model::Column::GroupTime,
                                hold_model::Column::Resolution,
                                hold_model::Column::Computed,
                            ])
                            .from(hold_model::Entity)
                            .and_where(Expr::col(hold_model::Column::Id).eq(id))
                            .and_where(
                                Expr::col(hold_model::Column::Status)
                                    .eq(HoldStatus::Pending.as_str()),
                            )
                            .lock(LockType::Update)
                            .take(),
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
                    .query_all_raw(built(
                        SeaQuery::select()
                            .column(readings::Column::ReplicateIndex)
                            .expr_as(Expr::cust("is_flagged IS TRUE"), Alias::new("flagged"))
                            .from(readings::Entity)
                            .and_where(Expr::col(readings::Column::StreamId).eq(stream_id))
                            .and_where(Expr::col(readings::Column::Time).eq(group_time))
                            .take(),
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
                let flagged = crate::routes::private::readings::service::record_many(
                    txn,
                    crate::routes::private::readings::models::Kind::Flag,
                    {
                        use crate::routes::private::collection_events::flows::row;
                        use crate::routes::private::readings::models::Column;
                        sea_orm::Condition::all()
                            .add(row(Column::StreamId).eq(stream_id))
                            .add(row(Column::Time).eq(group_time))
                            .add(row(Column::ReplicateIndex).is_in(indexes.clone()))
                            .add(
                                crate::routes::private::collection_events::flows::row_is_true(
                                    Column::IsFlagged,
                                    false,
                                ),
                            )
                    },
                    crate::routes::private::readings::service::NewValue::Literal(
                        serde_json::json!({ "reason": reason, "hold_id": id }),
                    ),
                    &by,
                    Some(&reason),
                    crate::routes::private::readings::models::Origin::Audit,
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
                let decided = decide_hold(
                    txn,
                    id,
                    &[HoldStatus::Pending],
                    HoldStatus::Remediated,
                    resolution,
                    Some(&by),
                )
                .await?;
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
                samples_affected: None,
            }))
        }
        // The two rule on whatever the hold is about: a measurement an intern entered, or the
        // field day they opened (Q21, Q177). The hold's own kind says which.
        "verify" | "reject" => {
            let kind = hold_model::Entity::find_by_id(id)
                .one(&state.db)
                .await?
                .ok_or_else(|| AppError::NotFound(format!("no replicate audit hold {id}")))?
                .kind;
            if kind == HoldKind::UnverifiedVisit.as_str() {
                rule_on_visit(&state, id, &payload.mode, payload.reason.as_deref(), &by).await
            } else {
                rule_on_entry(&state, id, &payload.mode, payload.reason.as_deref(), &by).await
            }
        }
        other => Err(AppError::BadRequest(format!(
            "unknown resolve mode '{other}'"
        ))),
    }
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
            .query_one_raw(built(
                hold_on_its_stream(JoinType::LeftJoin)
                    .column((HOLD, hold_model::Column::StreamId))
                    .column((HOLD, hold_model::Column::GroupTime))
                    .column((HOLD, hold_model::Column::Status))
                    .column((HOLD, hold_model::Column::Resolution))
                    .expr_as(
                        Expr::col((STREAM, data_streams::Column::SiteParameterId)).is_not_null(),
                        Alias::new("paired"),
                    )
                    .and_where(Expr::col((HOLD, hold_model::Column::Id)).eq(id))
                    .and_where(
                        Expr::col((HOLD, hold_model::Column::Status))
                            .is_in(HoldStatus::REOPENABLE.map(HoldStatus::as_str)),
                    )
                    .lock_with_tables(LockType::Update, [Alias::new(HOLD)])
                    .take(),
            ))
            .await?
            .ok_or_else(|| AppError::NotFound(format!("no decided replicate audit hold {id}")))?;
        let ReopenHoldRow {
            stream_id,
            group_time,
            status,
            paired,
            resolution: prev,
        } = ReopenHoldRow::from_query_result(&hold, "")?;

        let flagged = prev.as_ref().and_then(|r| {
            (r.get("action")? == "flag_replicates").then(|| {
                (
                    r.get("replicate_indexes")
                        .and_then(serde_json::Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(serde_json::Value::as_i64)
                                .filter_map(|i| i16::try_from(i).ok())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                    r.get("reason")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                )
            })
        });

        let reopened = super::service::status_for(paired);
        let resolution = merged_resolution(prev, serde_json::json!({"action": "reopened"}), &by);
        if status == HoldStatus::Remediated.as_str()
            && let Some((indexes, reason)) = &flagged
            && !indexes.is_empty()
        {
            // Only the rows this resolution flagged: a flag someone set since, or with another
            // reason, stays.
            crate::routes::private::readings::service::record_many(
                txn,
                crate::routes::private::readings::models::Kind::Unflag,
                {
                    use crate::routes::private::collection_events::flows::row;
                    use crate::routes::private::readings::models::Column;
                    sea_orm::Condition::all()
                        .add(row(Column::StreamId).eq(stream_id))
                        .add(row(Column::Time).eq(group_time))
                        .add(row(Column::ReplicateIndex).is_in(indexes.clone()))
                        .add(row(Column::IsFlagged).eq(true))
                        .add(row(Column::FlagReason).eq(reason.clone()))
                },
                crate::routes::private::readings::service::NewValue::Literal(
                    serde_json::json!({ "hold_id": id, "reopened": true }),
                ),
                &by,
                Some("reopened"),
                crate::routes::private::readings::models::Origin::Audit,
                Some(id),
            )
            .await?;
        }
        let restored =
            decide_hold(txn, id, &HoldStatus::REOPENABLE, reopened, resolution, None).await?;
        if restored != 1 {
            return Err(AppError::Conflict(format!(
                "replicate audit hold {id} changed under this request; no flag was reverted"
            )));
        }
        // The note said a decision had been taken here, and it has not any more.
        delete_audit_annotations(txn, id).await?;
        Ok(reopened.as_str().to_string())
    })
    .await?;
    state.response_cache.invalidate_all();
    Ok(Json(ResolveHoldResponse {
        status: reopened,
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
    let h = Alias::new("h");
    let hold_col = |c: hold_model::Column| Expr::col((h.clone(), c));
    let mut bounds = Condition::all();
    // A restricted caller acknowledges only holds whose stream is paired to a site in their
    // projects; unpaired (deferred) holds belong to no project and stay out of their reach.
    if let Some(projects) = scope.sql_project_array() {
        bounds = bounds.add(Expr::cust_with_values(
            "EXISTS (SELECT 1 FROM site_parameters sp JOIN sites st ON st.id = sp.site_id \
             WHERE sp.id = ds.site_parameter_id AND st.project_id = ANY($1))",
            [projects],
        ));
    }
    let stream_scoped = payload.stream_id.is_some() || payload.source_system.is_some();
    if let Some(stream_id) = payload.stream_id {
        bounds = bounds.add(hold_col(hold_model::Column::StreamId).eq(stream_id));
    }
    if let Some(source_system) = payload.source_system {
        bounds = bounds.add(Expr::cust_with_values(
            "ds.source_system = $1",
            [sea_orm::Value::from(source_system)],
        ));
    }
    if let Some(start) = payload.start {
        bounds = bounds.add(
            hold_col(hold_model::Column::GroupTime)
                .gte(sea_orm::prelude::DateTimeWithTimeZone::from(start)),
        );
    }
    if let Some(end) = payload.end {
        bounds = bounds.add(
            hold_col(hold_model::Column::GroupTime)
                .lte(sea_orm::prelude::DateTimeWithTimeZone::from(end)),
        );
    }
    if let Some(ceiling) = payload.max_relative_delta {
        bounds = bounds.add(super::service::relative_delta_expr().lte(ceiling));
    }
    if let Some(ceiling) = payload.max_mean_relative_delta {
        bounds = bounds.add(super::service::relative_delta_of("mean").lte(ceiling));
    }
    if let Some(ceiling) = payload.max_sd_relative_delta {
        bounds = bounds.add(super::service::relative_delta_of("sd").lte(ceiling));
    }
    // Holds carrying no stream are out of the acknowledging statement's reach: it joins
    // `data_streams`, and every filter this route takes is a replicate-statistics threshold. Count
    // them so a sweep states what it passed over instead of returning a total that reads as the
    // whole queue (B262). A call naming a stream or a source system asked for streams, so nothing
    // was passed over; only the instant bounds narrow the rest.
    let skipped_no_stream = if stream_scoped {
        0
    } else {
        use sea_orm::PaginatorTrait as _;
        let mut stream_less = hold_model::Entity::find()
            .filter(hold_model::Column::Status.eq(HoldStatus::Pending.as_str()))
            .filter(hold_model::Column::StreamId.is_null());
        if let Some(start) = payload.start {
            stream_less = stream_less.filter(hold_model::Column::GroupTime.gte(start));
        }
        if let Some(end) = payload.end {
            stream_less = stream_less.filter(hold_model::Column::GroupTime.lte(end));
        }
        stream_less.count(&state.db).await?
    };

    let ds = Alias::new("ds");
    let acknowledge = SeaQuery::update()
        .table(
            sea_orm::sea_query::IntoTableRef::into_table_ref(hold_model::Entity).alias(h.clone()),
        )
        .value(
            hold_model::Column::Status,
            Expr::val(HoldStatus::Acknowledged.as_str()),
        )
        .value(
            hold_model::Column::Resolution,
            crate::routes::private::sync::service::accept_ours_resolution(&by),
        )
        .value(hold_model::Column::AcknowledgedBy, Expr::val(by.clone()))
        .value(hold_model::Column::AcknowledgedAt, Expr::cust("NOW()"))
        .from(
            sea_orm::sea_query::IntoTableRef::into_table_ref(
                crate::routes::private::data_streams::models::Entity,
            )
            .alias(ds.clone()),
        )
        .cond_where(
            Condition::all()
                .add(
                    Expr::col((
                        ds.clone(),
                        crate::routes::private::data_streams::models::Column::Id,
                    ))
                    .equals((h.clone(), hold_model::Column::StreamId)),
                )
                .add(hold_col(hold_model::Column::Status).eq(HoldStatus::Pending.as_str()))
                .add(bounds),
        )
        .to_owned();
    let acknowledged = state
        .db
        .execute_raw(built(acknowledge))
        .await?
        .rows_affected();
    // One note per instant, as the single acknowledge writes: a sweep is many decisions, and each
    // one is about a value somebody may later look at on a chart. The insert reads the holds this
    // call just decided, identified by the actor and timestamp it stamped on them.
    if acknowledged > 0 {
        let note = Expr::cust_with_values(
            "'Audit accepted in bulk: the statistics computed here stand (source mean '
               || COALESCE(round((h.expected->>'mean')::numeric, 4)::text, 'none')
               || ' sd ' || COALESCE(round((h.expected->>'sd')::numeric, 4)::text, 'none')
               || ', recomputed mean '
               || COALESCE(round((h.computed->>'mean')::numeric, 4)::text, 'none')
               || ' sd ' || COALESCE(round((h.computed->>'sd')::numeric, 4)::text, 'none')
               || ' over ' || COALESCE(h.computed->>'n', '0')
               || ' replicates). Accepted by ' || $1 || '.'",
            [by.clone()],
        );
        let decided_here = hold_on_its_slot()
            .column((SLOT, site_parameters::Column::SiteId))
            .column((SLOT, site_parameters::Column::ParameterId))
            .column((HOLD, hold_model::Column::GroupTime))
            .column((HOLD, hold_model::Column::GroupTime))
            .expr(note)
            .expr(Expr::val(AUDIT_ANNOTATION_CATEGORY))
            .expr(Expr::val(by.clone()))
            .column((HOLD, hold_model::Column::Id))
            .and_where(
                Expr::col((HOLD, hold_model::Column::Status)).eq(HoldStatus::Acknowledged.as_str()),
            )
            .and_where(Expr::col((HOLD, hold_model::Column::AcknowledgedBy)).eq(by.clone()))
            .and_where(
                Expr::col((HOLD, hold_model::Column::AcknowledgedAt))
                    .gt(Expr::cust("NOW() - INTERVAL '1 minute'")),
            )
            .and_where(Expr::exists(not_yet_annotated()).not())
            .take();
        let mut annotate = SeaQuery::insert();
        annotate
            .into_table(annotations::Entity)
            .columns([
                annotations::Column::SiteId,
                annotations::Column::ParameterId,
                annotations::Column::StartTime,
                annotations::Column::EndTime,
                annotations::Column::Text,
                annotations::Column::Category,
                annotations::Column::CreatedBy,
                annotations::Column::AuditHoldId,
            ])
            .select_from(decided_here)
            .map_err(|e| AppError::Internal(e.to_string()))?;
        let annotated = state.db.execute_raw(built(annotate.to_owned())).await;
        if let Err(e) = annotated {
            tracing::warn!("could not annotate bulk-acknowledged holds: {e}");
        }
    }
    Ok(Json(AcknowledgeResponse {
        acknowledged,
        skipped_no_stream,
    }))
}

/// Sync admin views are split by required authorization, and each group carries its own layers
/// (Q143), so `service/mod.rs` mounts them under `/sync` and adds nothing. The layers used to sit
/// at the nest site, where `tests/route_guards.rs` could not see them and all 36 routes recorded
/// as unguarded (T97).
///
/// Group membership:
/// - `read_routes`: list/get operations, fine for any read_metadata caller.
/// - `write_routes`: operator actions such as issuing sync commands and pairing workflows.
///   Same gate as other entity mutations (Keycloak admin or write_metadata token).
/// - `manage_routes`: the replicate audit review surface.
///   Manager-level humans and above (or a write_metadata token); interns and plain members never
///   see the audit backlog.
/// - `admin_routes`: credential listing, creation and revoke, these mint full-permission
///   sync session tokens, so they're Keycloak-admin only (no API token can pass). The listing
///   is admin-gated alongside them, matching `sync_service_credentials` CRUD, so a leaked token
///   cannot enumerate which credentials exist.
pub fn read_routes() -> Router<AppState> {
    Router::new()
        .route("/pairing-plans", get(list_pairing_plans))
        .route("/pairing-plans/{id}", get(get_pairing_plan))
        .route("/pairing-plans/{id}/site-metadata", get(plan_site_metadata))
        .route("/pairing-plans/{id}/instruments", get(plan_instruments))
        .route("/unpaired-summary", get(unpaired_summary))
        .layer(middleware::from_fn(require_read_metadata))
}

pub fn write_routes() -> Router<AppState> {
    Router::new()
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
        .layer(RequestBodyLimitLayer::new(ACTION_BODY_LIMIT))
        .layer(middleware::from_fn(deny_scoped_token))
        // Human management of sync services is Administrator-only; write_metadata token preserved.
        .layer(middleware::from_fn(require_admin_or_token_write_metadata))
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
            "/change_proposals/decide",
            post(crate::routes::private::readings::views::decide_proposals),
        )
        .layer(RequestBodyLimitLayer::new(ACTION_BODY_LIMIT))
        .layer(middleware::from_fn(deny_scoped_token))
        .layer(middleware::from_fn(require_manage_sensors))
}

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/credentials", post(create_credential))
        .route("/credentials/{id}/revoke", post(revoke_credential))
        .layer(RequestBodyLimitLayer::new(ACTION_BODY_LIMIT))
        .layer(middleware::from_fn(require_admin))
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

    let catalog =
        crate::routes::private::sync::service::load_entity_catalog(&state.db, &plan.source_system)
            .await?;

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

    crate::routes::private::sync::service::apply_group_updates(
        &mut entries,
        &req.updates,
        &catalog,
    );
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
            proposal.attach_to = update.attach_to;
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
    // An unconfirmed instrument and an unchecked row are both the operator's decision, so the
    // refusal belongs in the response rather than in a failed job they have to go and read.
    // `apply_plan` checks both again: the job is reachable on its own.
    let plan = crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    let entries: Vec<crate::routes::private::sync::service::PlanEntry> = plan.entries.0;
    crate::routes::private::sync::service::refuse_unconfirmed_instruments(&entries)?;
    crate::routes::private::sync::service::refuse_unchecked_entries(&entries)?;

    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
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
    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
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
    let mut uses = Vec::new();
    for row in state
        .db
        .query_all_raw(built(
            SeaQuery::select()
                .column(readings::Column::StandardCurveId)
                .column(readings::Column::StreamId)
                .expr_as(Expr::cust("COUNT(*)"), Alias::new("n"))
                .expr_as(Expr::col(readings::Column::Time).min(), Alias::new("first"))
                .expr_as(Expr::col(readings::Column::Time).max(), Alias::new("last"))
                .from(readings::Entity)
                .and_where(Expr::col(readings::Column::StandardCurveId).is_not_null())
                .add_group_by([
                    Expr::col(readings::Column::StandardCurveId),
                    Expr::col(readings::Column::StreamId),
                ])
                .take(),
        ))
        .await?
    {
        uses.push(crate::routes::private::sync::service::CurveUse {
            curve_id: row.try_get("", "standard_curve_id")?,
            stream_id: row.try_get("", "stream_id")?,
            n: row.try_get("", "n")?,
            first: row.try_get("", "first")?,
            last: row.try_get("", "last")?,
        });
    }
    let mut reach = crate::routes::private::sync::service::curve_reach(&uses, &entries);
    let intents = crate::routes::private::sync::service::plan_curve_intents(&plan)?;
    let proposed_names: std::collections::HashMap<&str, &str> = entries
        .iter()
        .filter_map(|e| e.instrument.as_ref())
        .filter(|i| i.create)
        .map(|i| (i.source_key.as_str(), i.name.as_str()))
        .collect();
    let mut curves: Vec<PlanCurveAssignment> =
        crate::routes::private::standard_curves::Entity::find()
            .filter(
                crate::routes::private::standard_curves::Column::SourceSystem
                    .eq(plan.source_system.clone()),
            )
            .all(&state.db)
            .await?
            .into_iter()
            .map(|c| {
                let pending = intents.iter().find(|i| i.curve_id == c.id);
                let reach = reach.remove(&c.id).unwrap_or_default();
                PlanCurveAssignment {
                    instrument_name: instrument_names
                        .get(&c.sensor_id)
                        .cloned()
                        .unwrap_or_else(|| c.sensor_id.to_string()),
                    reading_count: reach.reading_count,
                    corrected_parameters: reach.parameters,
                    corrected_sites: reach.sites,
                    first_corrected: reach.first,
                    last_corrected: reach.last,
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
                instrument: entry.instrument.as_ref().map(|i| PlanInstrumentGroup {
                    scope: Some(instrument_key(entry)),
                    instrument_id: i.id,
                    name: i.name.clone(),
                    source_key: i.source_key.clone(),
                    resolved_by: i.resolved_by.clone(),
                    create: i.create,
                    confirmed: i.confirmed,
                    stamps_readings: i.stamps_readings,
                    curve_column: i.curve_column.clone(),
                    stream_count: 1,
                    parameters: vec![entry.parameter.name.clone()],
                    site_count: 1,
                    anchor_stream_id: Some(entry.stream_id),
                    curves: i.curves.clone(),
                    proposed_name: i.proposed_name.clone(),
                    // Read fresh, like the lab rows' catalog: an instrument created since the plan
                    // was drafted is the collision this reports.
                    name_conflict: if i.create {
                        catalog.named(&i.name)
                    } else {
                        None
                    },
                }),
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
