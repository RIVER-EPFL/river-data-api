use axum::{Json, Router, extract::{Path, State}, routing::{get, post}};
use chrono::Utc;
use sea_orm::{ActiveModelTrait, ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, Set, sea_query::Expr};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::{data_streams, parameters, projects, site_parameters, sites, sync_commands, sync_events, sync_service_credentials, sync_services, sync_service_tokens};
use crate::error::{AppError, AppResult};
use crate::services::api_token::{generate_token, hash_token};
use crate::services::operations::{create_sensor_for_stream, extract_vaisala_device_serial};

// ============================================================================
// Stream State (replaces old Sync State)
// ============================================================================

#[derive(Serialize)]
pub struct StreamStateResponse {
    pub id: Uuid,
    pub source_system: String,
    pub source_key: String,
    pub source_name: Option<String>,
    pub source_path: Option<String>,
    pub metadata: serde_json::Value,
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
        .route("/discovery", get(get_discovery))
        .route("/apply-discovery", post(apply_discovery))
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
            source_path: s.source_path,
            metadata: s.metadata,
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
    use sea_orm::QuerySelect;

    let commands: Vec<SyncCommandResponse> = sync_commands::Entity::find()
        .order_by_desc(sync_commands::Column::CreatedAt)
        .limit(50)
        .all(&state.db)
        .await?
        .into_iter()
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

#[derive(Debug, serde::Deserialize)]
struct SyncEventsQuery {
    #[serde(default = "default_page")]
    page: u64,
    #[serde(default = "default_per_page")]
    per_page: u64,
}

fn default_page() -> u64 { 1 }
fn default_per_page() -> u64 { 25 }

async fn list_sync_events(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<SyncEventsQuery>,
) -> AppResult<(axum::http::StatusCode, axum::http::HeaderMap, Json<Vec<SyncEventResponse>>)> {
    use sea_orm::PaginatorTrait;

    let per_page = params.per_page.min(100);
    let page = params.page.max(1) - 1; // 0-indexed for SeaORM

    let paginator = sync_events::Entity::find()
        .order_by_desc(sync_events::Column::StartedAt)
        .paginate(&state.db, per_page);

    let total = paginator.num_items().await?;
    let events = paginator.fetch_page(page).await?;

    let response: Vec<SyncEventResponse> = events
        .into_iter()
        .map(sync_event_to_response)
        .collect();

    let mut headers = axum::http::HeaderMap::new();
    let range_value = if response.is_empty() {
        format!("items */{total}")
    } else {
        let start = page * per_page;
        let end = start + response.len() as u64 - 1;
        format!("items {start}-{end}/{total}")
    };
    headers.insert(
        "Content-Range",
        range_value.parse().unwrap(),
    );
    headers.insert(
        "Access-Control-Expose-Headers",
        "Content-Range".parse().unwrap(),
    );

    Ok((axum::http::StatusCode::OK, headers, Json(response)))
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

// ============================================================================
// Stream Discovery & Auto-Pairing
// ============================================================================

#[derive(Serialize)]
pub struct DiscoveryMatch {
    pub id: Uuid,
    pub name: String,
}

#[derive(Serialize)]
pub struct DiscoverySuggestion {
    #[serde(rename = "match")]
    pub matched: Option<DiscoveryMatch>,
    pub confidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_units: Option<String>,
}

#[derive(Serialize)]
pub struct DiscoverySuggestions {
    pub project: DiscoverySuggestion,
    pub site: DiscoverySuggestion,
    pub parameter: DiscoverySuggestion,
    pub site_parameter: DiscoverySuggestion,
}

#[derive(Serialize)]
pub struct DiscoveryStreamInfo {
    pub id: Uuid,
    pub source_name: Option<String>,
    pub source_path: Option<String>,
    pub metadata: serde_json::Value,
}

#[derive(Serialize)]
pub struct DiscoverySensorInfo {
    pub existing_sensor_id: Option<Uuid>,
    pub vaisala_device_serial: Option<String>,
}

#[derive(Serialize)]
pub struct DiscoveryItem {
    pub stream: DiscoveryStreamInfo,
    pub suggestions: DiscoverySuggestions,
    pub sensor_info: DiscoverySensorInfo,
    pub action: String,
}

fn match_confidence(name: &str, candidates: &[(Uuid, String)]) -> DiscoverySuggestion {
    let lower = name.to_lowercase();
    // Exact case-insensitive match
    if let Some((id, cname)) = candidates.iter().find(|(_, n)| n.to_lowercase() == lower) {
        return DiscoverySuggestion {
            matched: Some(DiscoveryMatch { id: *id, name: cname.clone() }),
            confidence: "exact".to_string(),
            suggested_name: None,
            suggested_units: None,
        };
    }
    // Fuzzy: substring containment
    if let Some((id, cname)) = candidates.iter().find(|(_, n)| {
        n.to_lowercase().contains(&lower) || lower.contains(&n.to_lowercase())
    }) {
        return DiscoverySuggestion {
            matched: Some(DiscoveryMatch { id: *id, name: cname.clone() }),
            confidence: "fuzzy".to_string(),
            suggested_name: None,
            suggested_units: None,
        };
    }
    DiscoverySuggestion {
        matched: None,
        confidence: "none".to_string(),
        suggested_name: Some(name.to_string()),
        suggested_units: None,
    }
}

/// `GET /api/admin/sync/discovery` — returns structured discovery report for unpaired streams.
async fn get_discovery(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<DiscoveryItem>>> {
    let db = &state.db;

    // Fetch unpaired streams
    let streams = data_streams::Entity::find()
        .filter(data_streams::Column::SiteParameterId.is_null())
        .filter(data_streams::Column::IsActive.eq(true))
        .all(db)
        .await?;

    if streams.is_empty() {
        return Ok(Json(vec![]));
    }

    // Fetch all existing entities for matching
    let all_projects: Vec<(Uuid, String)> = projects::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();

    let all_sites: Vec<(Uuid, String, Option<Uuid>)> = sites::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.id, s.name, s.project_id))
        .collect();

    let all_params: Vec<(Uuid, String)> = parameters::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();

    let all_site_params: Vec<(Uuid, Uuid, Uuid)> = site_parameters::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|sp| (sp.id, sp.site_id, sp.parameter_id))
        .collect();

    let site_candidates: Vec<(Uuid, String)> = all_sites.iter().map(|(id, name, _)| (*id, name.clone())).collect();

    let mut items = Vec::new();

    for stream in streams {
        // Parse hierarchy from metadata or source_path
        let hierarchy = stream.metadata.get("hierarchy").cloned();
        let (project_name, site_name, param_name) = if let Some(h) = &hierarchy {
            (
                h.get("project").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                h.get("site").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                h.get("parameter").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            )
        } else if let Some(ref path) = stream.source_path {
            let segs: Vec<&str> = path.split('/').collect();
            (
                segs.get(1).unwrap_or(&"").to_string(),
                segs.get(2).unwrap_or(&"").to_string(),
                segs.get(3).unwrap_or(&"").to_string(),
            )
        } else {
            (String::new(), String::new(), stream.source_name.clone().unwrap_or_default())
        };

        let units = stream.metadata.get("units")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Match project
        let project_suggestion = if project_name.is_empty() {
            DiscoverySuggestion {
                matched: None,
                confidence: "none".to_string(),
                suggested_name: None,
                suggested_units: None,
            }
        } else {
            match_confidence(&project_name, &all_projects)
        };

        // Match site (prefer sites within matched project)
        let site_suggestion = if site_name.is_empty() {
            DiscoverySuggestion {
                matched: None,
                confidence: "none".to_string(),
                suggested_name: None,
                suggested_units: None,
            }
        } else {
            // Try to match within project first
            let project_id = project_suggestion.matched.as_ref().map(|m| m.id);
            let site_within_project: Vec<(Uuid, String)> = if let Some(pid) = project_id {
                all_sites.iter()
                    .filter(|(_, _, proj)| *proj == Some(pid))
                    .map(|(id, name, _)| (*id, name.clone()))
                    .collect()
            } else {
                vec![]
            };

            let suggestion = if !site_within_project.is_empty() {
                match_confidence(&site_name, &site_within_project)
            } else {
                match_confidence(&site_name, &site_candidates)
            };
            suggestion
        };

        // Match parameter
        let mut param_suggestion = if param_name.is_empty() {
            DiscoverySuggestion {
                matched: None,
                confidence: "none".to_string(),
                suggested_name: Some(stream.source_name.clone().unwrap_or_default()),
                suggested_units: if units.is_empty() { None } else { Some(units.clone()) },
            }
        } else {
            let mut s = match_confidence(&param_name, &all_params);
            if s.confidence == "none" {
                s.suggested_units = if units.is_empty() { None } else { Some(units.clone()) };
            }
            s
        };
        // Always set suggested_units on parameter if available and not already set
        if param_suggestion.suggested_units.is_none() && !units.is_empty() {
            param_suggestion.suggested_units = Some(units.clone());
        }

        // Match site_parameter
        let sp_suggestion = if let (Some(site_match), Some(param_match)) =
            (&site_suggestion.matched, &param_suggestion.matched)
        {
            if let Some(sp) = all_site_params.iter().find(|(_, sid, pid)| {
                *sid == site_match.id && *pid == param_match.id
            }) {
                DiscoverySuggestion {
                    matched: Some(DiscoveryMatch {
                        id: sp.0,
                        name: format!("{}:{}", site_match.name, param_match.name),
                    }),
                    confidence: "exact".to_string(),
                    suggested_name: None,
                    suggested_units: None,
                }
            } else {
                DiscoverySuggestion {
                    matched: None,
                    confidence: "none".to_string(),
                    suggested_name: None,
                    suggested_units: None,
                }
            }
        } else {
            DiscoverySuggestion {
                matched: None,
                confidence: "none".to_string(),
                suggested_name: None,
                suggested_units: None,
            }
        };

        // Determine action
        let action = if sp_suggestion.confidence == "exact" {
            "pair_existing"
        } else if project_suggestion.confidence != "none"
            && site_suggestion.confidence != "none"
        {
            "create_and_pair"
        } else {
            "needs_input"
        };

        let sensor_info = DiscoverySensorInfo {
            existing_sensor_id: stream.sensor_id,
            vaisala_device_serial: extract_vaisala_device_serial(&stream.metadata),
        };

        items.push(DiscoveryItem {
            stream: DiscoveryStreamInfo {
                id: stream.id,
                source_name: stream.source_name,
                source_path: stream.source_path,
                metadata: stream.metadata,
            },
            suggestions: DiscoverySuggestions {
                project: project_suggestion,
                site: site_suggestion,
                parameter: param_suggestion,
                site_parameter: sp_suggestion,
            },
            sensor_info,
            action: action.to_string(),
        });
    }

    Ok(Json(items))
}

// ============================================================================
// Apply Discovery
// ============================================================================

#[derive(Deserialize)]
pub struct ApplyDiscoveryRequest {
    pub actions: Vec<ApplyAction>,
}

#[derive(Deserialize)]
pub struct ApplyAction {
    pub stream_id: Uuid,
    #[serde(default)]
    pub create_project: Option<CreateProjectAction>,
    #[serde(default)]
    pub create_site: Option<CreateSiteAction>,
    #[serde(default)]
    pub create_parameter: Option<CreateParameterAction>,
    #[serde(default)]
    pub create_site_parameter: Option<CreateSiteParameterAction>,
    /// UUID of existing site_parameter, or "new" to auto-create from above fields
    pub pair_to: String,
    #[serde(default)]
    pub use_project_id: Option<Uuid>,
    #[serde(default)]
    pub use_site_id: Option<Uuid>,
    #[serde(default)]
    pub use_parameter_id: Option<Uuid>,
}

#[derive(Deserialize)]
pub struct CreateProjectAction {
    pub name: String,
}

#[derive(Deserialize)]
pub struct CreateSiteAction {
    pub name: String,
}

#[derive(Deserialize)]
pub struct CreateParameterAction {
    pub name: String,
    #[serde(default = "default_display_name")]
    pub display_name: String,
    #[serde(default)]
    pub default_units: String,
    #[serde(default = "default_category")]
    pub category: String,
}

fn default_display_name() -> String { String::new() }
fn default_category() -> String { "measurement".to_string() }

#[derive(Deserialize)]
pub struct CreateSiteParameterAction {
    #[serde(default)]
    pub display_units: Option<String>,
    #[serde(default)]
    pub sample_interval_sec: Option<i32>,
    #[serde(default)]
    pub channel_id: Option<i32>,
}

#[derive(Serialize)]
pub struct ApplyDiscoveryResponse {
    pub projects_created: u32,
    pub sites_created: u32,
    pub parameters_created: u32,
    pub site_parameters_created: u32,
    pub sensors_created: u32,
    pub streams_paired: u32,
    pub total_backfilled: u64,
    pub errors: Vec<String>,
}

struct ActionStats {
    projects_created: u32,
    sites_created: u32,
    parameters_created: u32,
    site_parameters_created: u32,
    sensors_created: u32,
    streams_paired: u32,
    backfilled: u64,
}

/// `POST /api/admin/sync/apply-discovery` — batch-processes discovery actions.
async fn apply_discovery(
    State(state): State<AppState>,
    Json(req): Json<ApplyDiscoveryRequest>,
) -> AppResult<Json<ApplyDiscoveryResponse>> {
    let db = &state.db;
    let mut resp = ApplyDiscoveryResponse {
        projects_created: 0,
        sites_created: 0,
        parameters_created: 0,
        site_parameters_created: 0,
        sensors_created: 0,
        streams_paired: 0,
        total_backfilled: 0,
        errors: vec![],
    };

    for action in req.actions {
        let result = process_action(db, &action).await;
        match result {
            Ok(stats) => {
                resp.projects_created += stats.projects_created;
                resp.sites_created += stats.sites_created;
                resp.parameters_created += stats.parameters_created;
                resp.site_parameters_created += stats.site_parameters_created;
                resp.sensors_created += stats.sensors_created;
                resp.streams_paired += stats.streams_paired;
                resp.total_backfilled += stats.backfilled;
            }
            Err(e) => {
                resp.errors.push(format!(
                    "Stream {}: {}",
                    action.stream_id, e
                ));
            }
        }
    }

    // Trigger aggregate refresh if any readings were backfilled
    if resp.total_backfilled > 0 {
        let db_clone = db.clone();
        tokio::spawn(async move {
            crate::services::sync_state::refresh_continuous_aggregates_full(&db_clone).await;
        });
    }

    Ok(Json(resp))
}

/// Process a single apply-discovery action.
async fn process_action(
    db: &sea_orm::DatabaseConnection,
    action: &ApplyAction,
) -> Result<ActionStats, String> {
    let mut projects_created = 0u32;
    let mut sites_created = 0u32;
    let mut params_created = 0u32;
    let mut site_params_created = 0u32;

    // Resolve or create project
    let project_id = if let Some(pid) = action.use_project_id {
        pid
    } else if let Some(ref cp) = action.create_project {
        // Try to find existing first (case-insensitive)
        let existing = projects::Entity::find()
            .filter(Expr::cust_with_values(
                "LOWER(name) = $1",
                [cp.name.to_lowercase()],
            ))
            .one(db)
            .await
            .map_err(|e| e.to_string())?;

        if let Some(existing) = existing {
            existing.id
        } else {
            let p = projects::ActiveModel {
                id: Set(Uuid::new_v4()),
                name: Set(cp.name.clone()),
                description: Set(None),
                data_source: Set("vaisala".to_string()),
                is_public: Set(false),
                public_slug: Set(None),
                public_api_title: Set(None),
                public_api_description: Set(None),
                public_api_version: Set(None),
                public_contact_email: Set(None),
                created_at: Set(Some(Utc::now())),
                discovered_at: Set(Some(Utc::now())),
            };
            let inserted = p.insert(db).await.map_err(|e| e.to_string())?;
            projects_created += 1;
            inserted.id
        }
    } else {
        return Err("No project specified".to_string());
    };

    // Resolve or create site
    let site_id = if let Some(sid) = action.use_site_id {
        sid
    } else if let Some(ref cs) = action.create_site {
        let existing = sites::Entity::find()
            .filter(Expr::cust_with_values(
                "LOWER(name) = $1",
                [cs.name.to_lowercase()],
            ))
            .one(db)
            .await
            .map_err(|e| e.to_string())?;

        if let Some(existing) = existing {
            existing.id
        } else {
            let s = sites::ActiveModel {
                id: Set(Uuid::new_v4()),
                project_id: Set(Some(project_id)),
                name: Set(cs.name.clone()),
                latitude: Set(None),
                longitude: Set(None),
                altitude_m: Set(None),
                public_slug: Set(None),
                created_at: Set(Some(Utc::now())),
                discovered_at: Set(Some(Utc::now())),
            };
            let inserted = s.insert(db).await.map_err(|e| e.to_string())?;
            sites_created += 1;
            inserted.id
        }
    } else {
        return Err("No site specified".to_string());
    };

    // Resolve or create parameter
    let parameter_id = if let Some(pid) = action.use_parameter_id {
        pid
    } else if let Some(ref cp) = action.create_parameter {
        let existing = parameters::Entity::find()
            .filter(Expr::cust_with_values(
                "LOWER(name) = $1",
                [cp.name.to_lowercase()],
            ))
            .one(db)
            .await
            .map_err(|e| e.to_string())?;

        if let Some(existing) = existing {
            existing.id
        } else {
            let display_name = if cp.display_name.is_empty() {
                cp.name.clone()
            } else {
                cp.display_name.clone()
            };
            let p = parameters::ActiveModel {
                id: Set(Uuid::new_v4()),
                name: Set(cp.name.clone()),
                display_name: Set(display_name),
                default_units: Set(cp.default_units.clone()),
                category: Set(cp.category.clone()),
                data_type: Set("numeric".to_string()),
                description: Set(None),
                default_warning_min: Set(None),
                default_warning_max: Set(None),
                default_alarm_min: Set(None),
                default_alarm_max: Set(None),
                created_at: Set(Some(Utc::now())),
            };
            let inserted = p.insert(db).await.map_err(|e| e.to_string())?;
            params_created += 1;
            inserted.id
        }
    } else {
        return Err("No parameter specified".to_string());
    };

    // Resolve or create site_parameter
    let site_parameter_id = if action.pair_to == "new" {
        // Check if already exists
        let existing = site_parameters::Entity::find()
            .filter(
                Condition::all()
                    .add(site_parameters::Column::SiteId.eq(site_id))
                    .add(site_parameters::Column::ParameterId.eq(parameter_id)),
            )
            .one(db)
            .await
            .map_err(|e| e.to_string())?;

        if let Some(existing) = existing {
            existing.id
        } else {
            let csp = &action.create_site_parameter;
            // Get the parameter name for the site_parameter name
            let param = parameters::Entity::find_by_id(parameter_id)
                .one(db)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("Parameter not found")?;

            let sp = site_parameters::ActiveModel {
                id: Set(Uuid::new_v4()),
                site_id: Set(site_id),
                parameter_id: Set(parameter_id),
                name: Set(param.name),
                sensor_type: Set(String::new()),
                display_units: Set(csp.as_ref().and_then(|c| c.display_units.clone())),
                units_name: Set(None),
                units_min: Set(None),
                units_max: Set(None),
                decimal_places: Set(None),
                channel_id: Set(csp.as_ref().and_then(|c| c.channel_id)),
                sample_interval_sec: Set(csp.as_ref().and_then(|c| c.sample_interval_sec)),
                is_active: Set(Some(true)),
                is_derived: Set(Some(false)),
                derived_definition_id: Set(None),
                variable_mappings: Set(None),
                created_at: Set(Some(Utc::now())),
                updated_at: Set(Some(Utc::now())),
                discovered_at: Set(Some(Utc::now())),
            };
            let inserted = sp.insert(db).await.map_err(|e| e.to_string())?;
            site_params_created += 1;
            inserted.id
        }
    } else {
        Uuid::parse_str(&action.pair_to).map_err(|_| "Invalid site_parameter_id".to_string())?
    };

    // Pair the stream
    let stream = data_streams::Entity::find_by_id(action.stream_id)
        .one(db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Stream not found")?;

    if stream.site_parameter_id.is_some() {
        return Err("Stream is already paired".to_string());
    }

    let sp = site_parameters::Entity::find_by_id(site_parameter_id)
        .one(db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Site parameter not found")?;

    // Create/reuse sensor for this stream
    let mut sensors_created = 0u32;
    let sensor_ctx = create_sensor_for_stream(db, &stream, sp.parameter_id, sp.site_id)
        .await
        .map_err(|e| e.to_string())?;
    if stream.sensor_id.is_none() {
        sensors_created = 1;
    }

    // Re-fetch stream (sensor_id may have been updated)
    let stream = data_streams::Entity::find_by_id(action.stream_id)
        .one(db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Stream not found after sensor creation")?;

    let now = Utc::now();
    let mut active: data_streams::ActiveModel = stream.into();
    active.site_parameter_id = Set(Some(site_parameter_id));
    active.paired_at = Set(Some(now.into()));
    active.updated_at = Set(now.into());
    active.update(db).await.map_err(|e| e.to_string())?;

    // Backfill readings with sensor context
    use sea_orm::{ConnectionTrait, Statement};
    let result = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings
              SET site_id = $1, parameter_id = $2,
                  sensor_id = $4, calibration_id = $5, deployment_id = $6,
                  calibrated_value = COALESCE(calibrated_value, raw_value)
              WHERE stream_id = $3 AND site_id IS NULL",
            [
                sp.site_id.into(),
                sp.parameter_id.into(),
                action.stream_id.into(),
                sensor_ctx.sensor_id.into(),
                sensor_ctx.calibration_id.into(),
                sensor_ctx.deployment_id.into(),
            ],
        ))
        .await
        .map_err(|e| e.to_string())?;

    let backfilled = result.rows_affected();

    // Backfill status_events too
    let _ = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE status_events
              SET site_id = $1, parameter_id = $2, sensor_id = $4
              WHERE stream_id = $3 AND site_id IS NULL",
            [
                sp.site_id.into(),
                sp.parameter_id.into(),
                action.stream_id.into(),
                sensor_ctx.sensor_id.into(),
            ],
        ))
        .await;

    Ok(ActionStats {
        projects_created,
        sites_created,
        parameters_created: params_created,
        site_parameters_created: site_params_created,
        sensors_created,
        streams_paired: 1,
        backfilled,
    })
}
