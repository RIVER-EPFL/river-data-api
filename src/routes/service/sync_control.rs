use axum::{Json, extract::{Path, State}};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, EntityTrait, QueryFilter, Set,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::{sync_commands, sync_events, sync_service_credentials, sync_service_tokens, sync_services};
use crate::error::{AppError, AppResult};
use crate::services::api_token::{generate_token, hash_token};

const SESSION_TOKEN_TTL_MINUTES: i64 = 15;

// ============================================================================
// Request / Response Types
// ============================================================================

#[derive(Deserialize)]
pub struct EnrollRequest {
    pub client_id: String,
    pub client_secret: String,
    pub instance_id: String,
}

#[derive(Serialize)]
pub struct EnrollResponse {
    pub service_id: Uuid,
    pub session_token: String,
}

#[derive(Deserialize)]
pub struct HeartbeatRequest {
    pub service_id: Uuid,
    pub client_secret: String,
    pub status: String,
    pub current_operation: Option<String>,
}

#[derive(Serialize)]
pub struct HeartbeatResponse {
    pub session_token: String,
    pub pending_commands: Vec<PendingCommandResponse>,
}

#[derive(Serialize)]
pub struct PendingCommandResponse {
    pub id: Uuid,
    pub command: String,
    pub payload: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct CommandUpdateRequest {
    pub status: String,
    pub result: Option<serde_json::Value>,
}

// ============================================================================
// Helpers
// ============================================================================

fn generate_session_token() -> (String, String) {
    let raw = generate_token();
    let hash = hash_token(&raw);
    (raw, hash)
}

async fn create_session_token(
    db: &sea_orm::DatabaseConnection,
    service_id: Uuid,
) -> AppResult<String> {
    let (raw_token, token_hash) = generate_session_token();

    let token = sync_service_tokens::ActiveModel {
        id: Set(Uuid::new_v4()),
        service_id: Set(service_id),
        token_hash: Set(token_hash),
        expires_at: Set((Utc::now() + chrono::Duration::minutes(SESSION_TOKEN_TTL_MINUTES)).into()),
        created_at: Set(Utc::now().into()),
    };
    token.insert(db).await?;

    // Fire-and-forget: clean up expired tokens for this service
    let db_clone = db.clone();
    tokio::spawn(async move {
        let _ = sync_service_tokens::Entity::delete_many()
            .filter(sync_service_tokens::Column::ServiceId.eq(service_id))
            .filter(sync_service_tokens::Column::ExpiresAt.lt(Utc::now()))
            .exec(&db_clone)
            .await;
    });

    Ok(raw_token)
}

// ============================================================================
// Handlers
// ============================================================================

pub async fn enroll(
    State(state): State<AppState>,
    Json(req): Json<EnrollRequest>,
) -> AppResult<Json<EnrollResponse>> {
    // Validate credentials
    let cred = sync_service_credentials::Entity::find()
        .filter(sync_service_credentials::Column::ClientId.eq(&req.client_id))
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::Unauthorized("Invalid client_id".to_string()))?;

    if cred.revoked {
        return Err(AppError::Unauthorized("Credentials have been revoked".to_string()));
    }

    let secret_hash = hash_token(&req.client_secret);
    if secret_hash != cred.client_secret_hash {
        return Err(AppError::Unauthorized("Invalid client_secret".to_string()));
    }

    // Upsert sync_services record
    let existing = sync_services::Entity::find()
        .filter(
            Condition::all()
                .add(sync_services::Column::ServiceType.eq(&cred.service_type))
                .add(sync_services::Column::InstanceId.eq(&req.instance_id)),
        )
        .one(&state.db)
        .await?;

    let service_id = if let Some(existing) = existing {
        // Update existing service
        let mut active: sync_services::ActiveModel = existing.clone().into();
        active.status = Set("starting".to_string());
        active.current_operation = Set(None);
        active.last_error = Set(None);
        active.updated_at = Set(Utc::now().into());
        active.update(&state.db).await?;
        existing.id
    } else {
        // Create new service
        let service = sync_services::ActiveModel {
            id: Set(Uuid::new_v4()),
            service_type: Set(cred.service_type.clone()),
            instance_id: Set(req.instance_id.clone()),
            status: Set("starting".to_string()),
            current_operation: Set(None),
            last_heartbeat: Set(None),
            last_sync_completed_at: Set(None),
            last_error: Set(None),
            created_at: Set(Utc::now().into()),
            updated_at: Set(Utc::now().into()),
        };
        let inserted = service.insert(&state.db).await?;
        inserted.id
    };

    // Link credential to service if not already linked
    if cred.service_id.is_none() {
        let mut cred_active: sync_service_credentials::ActiveModel = cred.into();
        cred_active.service_id = Set(Some(service_id));
        cred_active.update(&state.db).await?;
    }

    // Generate session token
    let session_token = create_session_token(&state.db, service_id).await?;

    Ok(Json(EnrollResponse {
        service_id,
        session_token,
    }))
}

pub async fn heartbeat(
    State(state): State<AppState>,
    Json(req): Json<HeartbeatRequest>,
) -> AppResult<Json<HeartbeatResponse>> {
    // Validate client_secret by finding the credential linked to this service
    let cred = sync_service_credentials::Entity::find()
        .filter(sync_service_credentials::Column::ServiceId.eq(req.service_id))
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::Unauthorized("No credential found for service".to_string()))?;

    if cred.revoked {
        return Err(AppError::Unauthorized("Credentials have been revoked".to_string()));
    }

    let secret_hash = hash_token(&req.client_secret);
    if secret_hash != cred.client_secret_hash {
        return Err(AppError::Unauthorized("Invalid client_secret".to_string()));
    }

    // Validate status
    const VALID_SERVICE_STATUSES: &[&str] = &["starting", "idle", "syncing", "error", "stopping"];
    if !VALID_SERVICE_STATUSES.contains(&req.status.as_str()) {
        return Err(AppError::BadRequest(format!(
            "Invalid status '{}'. Valid: {}",
            req.status,
            VALID_SERVICE_STATUSES.join(", ")
        )));
    }

    // Update service status
    let service = sync_services::Entity::find_by_id(req.service_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Service not found".to_string()))?;

    let mut active: sync_services::ActiveModel = service.into();
    active.status = Set(req.status);
    active.current_operation = Set(req.current_operation);
    active.last_heartbeat = Set(Some(Utc::now().into()));
    active.updated_at = Set(Utc::now().into());
    active.update(&state.db).await?;

    // Generate new session token
    let session_token = create_session_token(&state.db, req.service_id).await?;

    // Fetch pending commands
    let pending = sync_commands::Entity::find()
        .filter(
            Condition::all()
                .add(sync_commands::Column::ServiceId.eq(req.service_id))
                .add(sync_commands::Column::Status.eq("pending"))
                .add(sync_commands::Column::ExpiresAt.gt(Utc::now())),
        )
        .all(&state.db)
        .await?;

    let pending_commands = pending
        .into_iter()
        .map(|c| PendingCommandResponse {
            id: c.id,
            command: c.command,
            payload: c.payload,
        })
        .collect();

    // Fire-and-forget: expire old pending commands
    let db_clone = state.db.clone();
    let sid = req.service_id;
    tokio::spawn(async move {
        use sea_orm::ConnectionTrait;
        let _ = db_clone.execute(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE sync_commands SET status = 'expired' WHERE service_id = $1 AND status = 'pending' AND expires_at < NOW()",
            [sid.into()],
        )).await;
    });

    Ok(Json(HeartbeatResponse {
        session_token,
        pending_commands,
    }))
}

pub async fn update_command(
    State(state): State<AppState>,
    Path(command_id): Path<Uuid>,
    Json(req): Json<CommandUpdateRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let cmd = sync_commands::Entity::find_by_id(command_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Command not found".to_string()))?;

    let valid_statuses = ["acknowledged", "completed", "failed"];
    if !valid_statuses.contains(&req.status.as_str()) {
        return Err(AppError::BadRequest(format!(
            "Invalid status '{}'. Valid: {}",
            req.status,
            valid_statuses.join(", ")
        )));
    }

    let mut active: sync_commands::ActiveModel = cmd.into();
    active.status = Set(req.status.clone());
    if req.result.is_some() {
        active.result = Set(req.result);
    }
    if req.status == "acknowledged" {
        active.acknowledged_at = Set(Some(Utc::now().into()));
    }
    if req.status == "completed" || req.status == "failed" {
        active.completed_at = Set(Some(Utc::now().into()));
    }
    active.update(&state.db).await?;

    Ok(Json(serde_json::json!({"updated": true})))
}

// ============================================================================
// Sync Events
// ============================================================================

#[derive(Deserialize)]
pub struct CreateSyncEventRequest {
    pub service_id: Uuid,
    pub command_id: Option<Uuid>,
    pub event_type: Option<String>,
    pub status: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateSyncEventRequest {
    pub status: Option<String>,
    pub readings_synced: Option<i64>,
    pub status_events_synced: Option<i64>,
    pub errors: Option<serde_json::Value>,
    pub log: Option<serde_json::Value>,
    pub duration_ms: Option<i64>,
}

pub async fn create_sync_event(
    State(state): State<AppState>,
    Json(req): Json<CreateSyncEventRequest>,
) -> AppResult<Json<serde_json::Value>> {
    const VALID_EVENT_TYPES: &[&str] = &["scheduled", "manual", "command"];
    const VALID_EVENT_STATUSES: &[&str] = &["running", "completed", "failed", "cancelled"];

    if let Some(ref event_type) = req.event_type {
        if !VALID_EVENT_TYPES.contains(&event_type.as_str()) {
            return Err(AppError::BadRequest(format!(
                "Invalid event_type '{}'. Valid: {}",
                event_type,
                VALID_EVENT_TYPES.join(", ")
            )));
        }
    }

    if let Some(ref status) = req.status {
        if !VALID_EVENT_STATUSES.contains(&status.as_str()) {
            return Err(AppError::BadRequest(format!(
                "Invalid status '{}'. Valid: {}",
                status,
                VALID_EVENT_STATUSES.join(", ")
            )));
        }
    }

    let event = sync_events::ActiveModel {
        id: Set(Uuid::new_v4()),
        service_id: Set(req.service_id),
        command_id: Set(req.command_id),
        event_type: Set(req.event_type.unwrap_or_else(|| "scheduled".to_string())),
        status: Set(req.status.unwrap_or_else(|| "running".to_string())),
        readings_synced: Set(0),
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

pub async fn update_sync_event(
    State(state): State<AppState>,
    Path(event_id): Path<Uuid>,
    Json(req): Json<UpdateSyncEventRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let event = sync_events::Entity::find_by_id(event_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Sync event not found".to_string()))?;

    let mut active: sync_events::ActiveModel = event.into();

    if let Some(status) = req.status {
        active.status = Set(status);
    }
    if let Some(readings) = req.readings_synced {
        active.readings_synced = Set(readings);
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
    active.completed_at = Set(Some(Utc::now().into()));

    active.update(&state.db).await?;

    Ok(Json(serde_json::json!({"updated": true})))
}
