use axum::{Json, Router, extract::{Path, State}, routing::{get, post}};
use chrono::Utc;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::{data_streams, sync_commands, sync_events, sync_service_credentials, sync_services, sync_service_tokens};
use crate::error::{AppError, AppResult};
use crate::services::api_token::{generate_token, hash_token};

// ============================================================================
// Stream State (replaces old Sync State)
// ============================================================================

#[derive(Serialize)]
pub struct StreamStateResponse {
    pub id: Uuid,
    pub source_system: String,
    pub source_key: String,
    pub source_name: Option<String>,
    pub site_parameter_id: Option<Uuid>,
    pub is_active: bool,
    pub last_data_time: Option<String>,
}

// ============================================================================
// Sync Services
// ============================================================================

#[derive(Serialize)]
pub struct SyncServiceResponse {
    pub id: Uuid,
    pub service_type: String,
    pub instance_id: String,
    pub status: String,
    pub current_operation: Option<String>,
    pub last_heartbeat: Option<String>,
    pub last_sync_completed_at: Option<String>,
    pub last_error: Option<String>,
    pub health: String,
    pub created_at: String,
    pub updated_at: String,
}

fn compute_health(last_heartbeat: Option<chrono::DateTime<chrono::FixedOffset>>) -> String {
    match last_heartbeat {
        None => "unknown".to_string(),
        Some(hb) => {
            let age = Utc::now() - hb.with_timezone(&Utc);
            if age.num_seconds() < 90 {
                "healthy".to_string()
            } else if age.num_seconds() < 300 {
                "warning".to_string()
            } else {
                "stale".to_string()
            }
        }
    }
}

fn service_to_response(s: sync_services::Model) -> SyncServiceResponse {
    let health = compute_health(s.last_heartbeat);
    SyncServiceResponse {
        id: s.id,
        service_type: s.service_type,
        instance_id: s.instance_id,
        status: s.status,
        current_operation: s.current_operation,
        last_heartbeat: s.last_heartbeat.map(|t| t.to_rfc3339()),
        last_sync_completed_at: s.last_sync_completed_at.map(|t| t.to_rfc3339()),
        last_error: s.last_error,
        health,
        created_at: s.created_at.to_rfc3339(),
        updated_at: s.updated_at.to_rfc3339(),
    }
}

// ============================================================================
// Sync Commands
// ============================================================================

#[derive(Deserialize)]
pub struct IssueCommandRequest {
    pub command: String,
    pub payload: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub struct SyncCommandResponse {
    pub id: Uuid,
    pub service_id: Uuid,
    pub command: String,
    pub payload: Option<serde_json::Value>,
    pub status: String,
    pub result: Option<serde_json::Value>,
    pub created_at: String,
    pub expires_at: String,
    pub acknowledged_at: Option<String>,
    pub completed_at: Option<String>,
}

fn command_to_response(c: sync_commands::Model) -> SyncCommandResponse {
    SyncCommandResponse {
        id: c.id,
        service_id: c.service_id,
        command: c.command,
        payload: c.payload,
        status: c.status,
        result: c.result,
        created_at: c.created_at.to_rfc3339(),
        expires_at: c.expires_at.to_rfc3339(),
        acknowledged_at: c.acknowledged_at.map(|t| t.to_rfc3339()),
        completed_at: c.completed_at.map(|t| t.to_rfc3339()),
    }
}

// ============================================================================
// Service Credentials
// ============================================================================

#[derive(Deserialize)]
pub struct CreateCredentialRequest {
    pub service_type: String,
}

#[derive(Serialize)]
pub struct CreateCredentialResponse {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Serialize)]
pub struct CredentialResponse {
    pub id: Uuid,
    pub client_id: String,
    pub service_type: String,
    pub service_id: Option<Uuid>,
    pub revoked: bool,
    pub created_at: String,
}

// ============================================================================
// Router
// ============================================================================

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/state", get(list_sync_states))
        .route("/services", get(list_services))
        .route("/services/{id}", get(get_service))
        .route("/services/{id}/commands", post(issue_command))
        .route("/services/{id}/revoke", post(revoke_service))
        .route("/commands", get(list_commands))
        .route("/events", get(list_sync_events))
        .route("/credentials", get(list_credentials).post(create_credential))
        .route("/credentials/{id}/revoke", post(revoke_credential))
}

// ============================================================================
// Handlers
// ============================================================================

async fn list_sync_states(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<StreamStateResponse>>> {
    let streams = data_streams::Entity::find()
        .order_by_asc(data_streams::Column::SourceSystem)
        .all(&state.db)
        .await?;

    let response: Vec<StreamStateResponse> = streams
        .into_iter()
        .map(|s| StreamStateResponse {
            id: s.id,
            source_system: s.source_system,
            source_key: s.source_key,
            source_name: s.source_name,
            site_parameter_id: s.site_parameter_id,
            is_active: s.is_active,
            last_data_time: s.last_data_time.map(|t| t.to_rfc3339()),
        })
        .collect();

    Ok(Json(response))
}

async fn list_services(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<SyncServiceResponse>>> {
    let services = sync_services::Entity::find()
        .order_by_desc(sync_services::Column::UpdatedAt)
        .all(&state.db)
        .await?;

    Ok(Json(services.into_iter().map(service_to_response).collect()))
}

async fn get_service(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<SyncServiceResponse>> {
    let service = sync_services::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Service not found".to_string()))?;

    Ok(Json(service_to_response(service)))
}

async fn issue_command(
    State(state): State<AppState>,
    Path(service_id): Path<Uuid>,
    Json(req): Json<IssueCommandRequest>,
) -> AppResult<Json<SyncCommandResponse>> {
    // Verify service exists
    sync_services::Entity::find_by_id(service_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Service not found".to_string()))?;

    let valid_commands = ["trigger_sync", "trigger_full_sync", "pause", "resume"];
    if !valid_commands.contains(&req.command.as_str()) {
        return Err(AppError::BadRequest(format!(
            "Invalid command '{}'. Valid commands: {}",
            req.command,
            valid_commands.join(", ")
        )));
    }

    let cmd = sync_commands::ActiveModel {
        id: Set(Uuid::new_v4()),
        service_id: Set(service_id),
        command: Set(req.command),
        payload: Set(req.payload),
        status: Set("pending".to_string()),
        result: Set(None),
        created_at: Set(Utc::now().into()),
        expires_at: Set((Utc::now() + chrono::Duration::minutes(5)).into()),
        acknowledged_at: Set(None),
        completed_at: Set(None),
    };

    let inserted = cmd.insert(&state.db).await?;
    Ok(Json(command_to_response(inserted)))
}

async fn list_commands(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<SyncCommandResponse>>> {
    let commands = sync_commands::Entity::find()
        .order_by_desc(sync_commands::Column::CreatedAt)
        .all(&state.db)
        .await?;

    // Return last 50 commands
    let commands: Vec<SyncCommandResponse> = commands
        .into_iter()
        .take(50)
        .map(command_to_response)
        .collect();

    Ok(Json(commands))
}

async fn create_credential(
    State(state): State<AppState>,
    Json(req): Json<CreateCredentialRequest>,
) -> AppResult<Json<CreateCredentialResponse>> {
    let client_id = format!("svc_{}", &generate_token()[..16]);
    let client_secret = generate_token();
    let secret_hash = hash_token(&client_secret);

    let cred = sync_service_credentials::ActiveModel {
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

async fn list_credentials(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<CredentialResponse>>> {
    let creds = sync_service_credentials::Entity::find()
        .order_by_desc(sync_service_credentials::Column::CreatedAt)
        .all(&state.db)
        .await?;

    Ok(Json(
        creds
            .into_iter()
            .map(|c| CredentialResponse {
                id: c.id,
                client_id: c.client_id,
                service_type: c.service_type,
                service_id: c.service_id,
                revoked: c.revoked,
                created_at: c.created_at.to_rfc3339(),
            })
            .collect(),
    ))
}

async fn revoke_credential(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    let cred = sync_service_credentials::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Credential not found".to_string()))?;

    // Mark credential as revoked
    let mut active: sync_service_credentials::ActiveModel = cred.into();
    active.revoked = Set(true);
    active.update(&state.db).await?;

    Ok(Json(serde_json::json!({"revoked": true})))
}

// ============================================================================
// Sync Events
// ============================================================================

#[derive(Serialize)]
pub struct SyncEventResponse {
    pub id: Uuid,
    pub service_id: Uuid,
    pub command_id: Option<Uuid>,
    pub event_type: String,
    pub status: String,
    pub readings_synced: i64,
    pub status_events_synced: i64,
    pub errors: Option<serde_json::Value>,
    pub log: Option<serde_json::Value>,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub duration_ms: Option<i64>,
}

fn sync_event_to_response(e: sync_events::Model) -> SyncEventResponse {
    SyncEventResponse {
        id: e.id,
        service_id: e.service_id,
        command_id: e.command_id,
        event_type: e.event_type,
        status: e.status,
        readings_synced: e.readings_synced,
        status_events_synced: e.status_events_synced,
        errors: e.errors,
        log: e.log,
        started_at: e.started_at.to_rfc3339(),
        completed_at: e.completed_at.map(|t| t.to_rfc3339()),
        duration_ms: e.duration_ms,
    }
}

async fn list_sync_events(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<SyncEventResponse>>> {
    let events = sync_events::Entity::find()
        .order_by_desc(sync_events::Column::StartedAt)
        .all(&state.db)
        .await?;

    let response: Vec<SyncEventResponse> = events
        .into_iter()
        .take(100)
        .map(sync_event_to_response)
        .collect();

    Ok(Json(response))
}

// ============================================================================
// Revoke Service
// ============================================================================

async fn revoke_service(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    // Find credential linked to this service
    let cred = sync_service_credentials::Entity::find()
        .filter(sync_service_credentials::Column::ServiceId.eq(id))
        .one(&state.db)
        .await?;

    if let Some(cred) = cred {
        let mut active: sync_service_credentials::ActiveModel = cred.into();
        active.revoked = Set(true);
        active.update(&state.db).await?;
    }

    // Delete all session tokens for this service
    sync_service_tokens::Entity::delete_many()
        .filter(sync_service_tokens::Column::ServiceId.eq(id))
        .exec(&state.db)
        .await?;

    Ok(Json(serde_json::json!({"revoked": true})))
}
