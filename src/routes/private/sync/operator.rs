use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use chrono::Utc;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set, sea_query::Expr};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use river_data_core::commands;
use river_data_core::models::CommandStatus;

use crate::common::AppState;
use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::routes::private::api_tokens::service::hash_token;
use crate::routes::private::sync::control::tokens::generate_token;
use super::{
    commands_model as sync_commands, credentials_model as sync_service_credentials,
    events_model as sync_events, services_model as sync_services, tokens_model as sync_service_tokens,
};

/// `Content-Range: items {start}-{end}/{total}` with an inclusive end, per RFC 7233, plus the
/// CORS expose header a browser needs to read it. Both list endpoints share this so their
/// pagination cannot drift apart.
fn content_range_headers(page: u64, per_page: u64, returned: usize, total: u64) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let range_value = if returned == 0 {
        format!("items */{total}")
    } else {
        let start = page * per_page;
        let end = start + returned as u64 - 1;
        format!("items {start}-{end}/{total}")
    };
    if let Ok(value) = range_value.parse() {
        headers.insert("Content-Range", value);
    }
    if let Ok(value) = "Content-Range".parse() {
        headers.insert("Access-Control-Expose-Headers", value);
    }
    headers
}

// ============================================================================
// Response Types
// ============================================================================

#[derive(Serialize, utoipa::ToSchema)]
pub struct SyncServiceResponse {
    pub id: Uuid,
    pub service_type: String,
    pub instance_id: String,
    pub status: String,
    pub paused: bool,
    pub current_operation: Option<String>,
    pub last_heartbeat: Option<String>,
    pub last_sync_completed_at: Option<String>,
    pub last_error: Option<String>,
    pub health: String,
    pub created_at: String,
    pub updated_at: String,
}

fn compute_health(
    last_heartbeat: Option<chrono::DateTime<chrono::FixedOffset>>,
    config: &Config,
) -> String {
    match last_heartbeat {
        None => "unknown".to_string(),
        Some(hb) => {
            let age = Utc::now() - hb.with_timezone(&Utc);
            if age.num_seconds() < config.sync_health_healthy_secs {
                "healthy".to_string()
            } else if age.num_seconds() < config.sync_health_warning_secs {
                "warning".to_string()
            } else {
                "stale".to_string()
            }
        }
    }
}

fn service_to_response(s: sync_services::Model, config: &Config) -> SyncServiceResponse {
    let health = compute_health(s.last_heartbeat, config);
    SyncServiceResponse {
        id: s.id,
        service_type: s.service_type,
        instance_id: s.instance_id,
        status: s.status,
        paused: s.paused,
        current_operation: s.current_operation,
        last_heartbeat: s.last_heartbeat.map(|t| t.to_rfc3339()),
        last_sync_completed_at: s.last_sync_completed_at.map(|t| t.to_rfc3339()),
        last_error: s.last_error,
        health,
        created_at: s.created_at.to_rfc3339(),
        updated_at: s.updated_at.to_rfc3339(),
    }
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct SyncCommandResponse {
    pub id: Uuid,
    pub service_id: Uuid,
    pub command: String,
    #[schema(value_type = Object)]
    pub payload: Option<serde_json::Value>,
    pub status: String,
    #[schema(value_type = Object)]
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

#[derive(Deserialize, utoipa::ToSchema)]
pub struct IssueCommandRequest {
    pub command: String,
    #[schema(value_type = Object)]
    pub payload: Option<serde_json::Value>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateCredentialRequest {
    pub service_type: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct CreateCredentialResponse {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct CredentialResponse {
    pub id: Uuid,
    pub client_id: String,
    pub service_type: String,
    pub service_id: Option<Uuid>,
    pub revoked: bool,
    pub created_at: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct SyncEventResponse {
    pub id: Uuid,
    pub service_id: Uuid,
    pub command_id: Option<Uuid>,
    pub event_type: String,
    pub status: String,
    pub readings_synced: i64,
    pub status_events_synced: i64,
    #[schema(value_type = Object)]
    pub errors: Option<serde_json::Value>,
    #[schema(value_type = Object)]
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

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct PaginationQuery {
    #[serde(default = "default_page")]
    pub page: u64,
    #[serde(default = "default_per_page")]
    pub per_page: u64,
}

fn default_page() -> u64 {
    1
}
fn default_per_page() -> u64 {
    25
}

/// Largest page either listing will serve.
const MAX_PER_PAGE: u64 = 100;

impl PaginationQuery {
    /// The zero-based page index and page size to query with.
    ///
    /// An oversized page is clamped, but a page size of zero is refused: `Paginator::paginate`
    /// asserts a non-zero size, so a silent clamp would turn a caller's mistake into a page they
    /// did not ask for while a panic would take the request down with no answer.
    fn resolve(&self) -> AppResult<(u64, u64)> {
        if self.per_page == 0 {
            return Err(AppError::BadRequest(
                "per_page must be at least 1".to_string(),
            ));
        }
        Ok((self.page.max(1) - 1, self.per_page.min(MAX_PER_PAGE)))
    }
}

// ============================================================================
// Handlers
// ============================================================================

/// List all registered sync services with their health (computed from `last_heartbeat`
/// age vs the `sync_health_healthy_secs`/`sync_health_warning_secs` thresholds in `Config`).
/// Sorted by `updated_at` DESC. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/services",
    responses(
        (status = 200, description = "Registered sync services with health", body = [SyncServiceResponse]),
    ),
    tag = "sync"
)]
pub async fn list_services(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<SyncServiceResponse>>> {
    let services = sync_services::Entity::find()
        .order_by_desc(sync_services::Column::UpdatedAt)
        .all(&state.db)
        .await?;

    let config = state.config.as_ref();
    Ok(Json(services.into_iter().map(|s| service_to_response(s, config)).collect()))
}

/// Get a single sync service by ID with its computed health. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/services/{id}",
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
    let service = sync_services::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Service not found".to_string()))?;

    Ok(Json(service_to_response(service, state.config.as_ref())))
}

/// Queue a command for a sync service. The command is picked up on the next heartbeat
/// (within `command_expiry_secs`). Valid commands: `trigger_sync`, `trigger_full_sync`,
/// `pause`, `resume`. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/services/{id}/commands",
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
    let service = sync_services::Entity::find_by_id(service_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Service not found".to_string()))?;

    let valid_commands = [
        commands::TRIGGER_SYNC,
        commands::TRIGGER_FULL_SYNC,
        commands::PAUSE,
        commands::RESUME,
    ];
    if !valid_commands.contains(&req.command.as_str()) {
        return Err(AppError::BadRequest(format!(
            "Invalid command '{}'. Valid commands: {}",
            req.command,
            valid_commands.join(", ")
        )));
    }

    // Persist the desired pause state at issue time so it takes effect even if
    // the service is down and never acknowledges the command.
    let desired_pause = match req.command.as_str() {
        commands::PAUSE => Some(true),
        commands::RESUME => Some(false),
        _ => None,
    };
    if let Some(paused) = desired_pause {
        let mut active: sync_services::ActiveModel = service.into();
        active.paused = Set(paused);
        active.updated_at = Set(Utc::now().into());
        active.update(&state.db).await?;
    }

    let expiry_secs = state.config.as_ref().sync_command_expiry_secs as i64;
    let cmd = sync_commands::ActiveModel {
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

/// Paginated list of sync commands (newest first). Returns a `Content-Range: items {start}-{end}/{total}`
/// header for React-admin style pagination. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/commands",
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

    let (page, per_page) = params.resolve()?;

    let paginator = sync_commands::Entity::find()
        .order_by_desc(sync_commands::Column::CreatedAt)
        .paginate(&state.db, per_page);

    let total = paginator.num_items().await?;
    let commands: Vec<SyncCommandResponse> = paginator
        .fetch_page(page)
        .await?
        .into_iter()
        .map(command_to_response)
        .collect();

    let headers = content_range_headers(page, per_page, commands.len(), total);

    Ok((StatusCode::OK, headers, Json(commands)))
}

/// Mint a new enrollment credential (client_id + client_secret). The `client_secret` is
/// returned in plaintext exactly ONCE, only the SHA-256 hash is stored. Used to bootstrap
/// a new sync service instance. Gated by `require_admin` upstream (Keycloak Administrator
/// only, no API token can pass).
#[utoipa::path(
    post,
    path = "/credentials",
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

/// List enrollment credentials with their service binding and revocation status. The
/// client_secret is never returned here, only the hash is stored. Gated by `require_admin`
/// upstream, matching credential mint and revoke: a credential bootstraps a full-permission
/// sync session token, so no API token may enumerate them.
#[utoipa::path(
    get,
    path = "/credentials",
    responses(
        (status = 200, description = "Credentials list (no secrets)", body = [CredentialResponse]),
    ),
    tag = "sync"
)]
pub async fn list_credentials(
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

/// Revoke an enrollment credential and immediately invalidate every active session
/// token bound to its service. Subsequent heartbeat or command updates will be rejected
/// as 401. Requires Keycloak Administrator (`require_admin` upstream).
#[utoipa::path(
    post,
    path = "/credentials/{id}/revoke",
    params(("id" = Uuid, Path, description = "Credential UUID")),
    responses(
        (status = 200, description = "Credential revoked, active sessions terminated"),
        (status = 404, description = "Credential not found"),
    ),
    tag = "sync"
)]
pub async fn revoke_credential(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    let cred = sync_service_credentials::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Credential not found".to_string()))?;

    let mut active: sync_service_credentials::ActiveModel = cred.clone().into();
    active.revoked = Set(true);
    active.update(&state.db).await?;

    if let Some(service_id) = cred.service_id {
        sync_service_tokens::Entity::delete_many()
            .filter(sync_service_tokens::Column::ServiceId.eq(service_id))
            .exec(&state.db)
            .await?;
    }

    Ok(Json(serde_json::json!({"revoked": true})))
}

/// Paginated list of sync events (newest first). Returns a `Content-Range` header for
/// React-admin style pagination. Each event records readings/status_events_synced counts,
/// optional errors/log JSON payloads, and duration. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/events",
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

    let (page, per_page) = params.resolve()?;

    let paginator = sync_events::Entity::find()
        .order_by_desc(sync_events::Column::StartedAt)
        .paginate(&state.db, per_page);

    let total = paginator.num_items().await?;
    let events = paginator.fetch_page(page).await?;

    let response: Vec<SyncEventResponse> =
        events.into_iter().map(sync_event_to_response).collect();

    let headers = content_range_headers(page, per_page, response.len(), total);

    Ok((StatusCode::OK, headers, Json(response)))
}

/// Revoke a sync service: marks every credential bound to it as revoked AND deletes
/// every active session token. The service is effectively forced off the control plane
/// until a new credential is minted. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/services/{id}/revoke",
    params(("id" = Uuid, Path, description = "Sync service UUID")),
    responses(
        (status = 200, description = "Service revoked"),
    ),
    tag = "sync"
)]
pub async fn revoke_service(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    sync_service_credentials::Entity::update_many()
        .col_expr(
            sync_service_credentials::Column::Revoked,
            Expr::value(true),
        )
        .filter(sync_service_credentials::Column::ServiceId.eq(id))
        .exec(&state.db)
        .await?;

    sync_service_tokens::Entity::delete_many()
        .filter(sync_service_tokens::Column::ServiceId.eq(id))
        .exec(&state.db)
        .await?;

    Ok(Json(serde_json::json!({"revoked": true})))
}
