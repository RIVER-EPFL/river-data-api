use axum::{Json, Router, extract::{Path, State}, routing::{get, post}};
use chrono::Utc;
use sea_orm::{ActiveModelTrait, ColumnTrait, ConnectionTrait, Condition, EntityTrait, QueryFilter, Set, TransactionTrait, sea_query::Expr};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::{data_streams, parameters, projects, site_parameters, sites};
use crate::error::{AppError, AppResult};
use crate::services::operations::{create_sensor_for_stream, extract_vaisala_device_serial};

// ============================================================================
// Router — library admin routes + local discovery/pairing routes
// ============================================================================

pub fn router() -> Router<AppState> {
    let (_service_routes, admin_routes) = river_data_core::server::routes::<AppState>();

    admin_routes
        .route("/discovery", get(get_discovery))
        .route("/apply-discovery", post(apply_discovery))
        .route("/grouped-discovery", post(grouped_discovery))
        .route("/bulk-pair", post(bulk_pair))
        .route("/pairing-plans", get(list_pairing_plans).post(create_pairing_plan))
        .route("/pairing-plans/{id}", get(get_pairing_plan).patch(update_pairing_plan))
        .route("/pairing-plans/{id}/apply", post(apply_pairing_plan))
        .route("/pairing-plans/{id}/revert", post(revert_pairing_plan))
        .route("/pairing-plans/{id}/site-metadata", get(plan_site_metadata))
        .route("/unpaired-summary", get(unpaired_summary))
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
///
/// Runs all actions within a single database transaction so that partial failures
/// don't leave orphaned entities or inconsistent pairing state.
async fn apply_discovery(
    State(state): State<AppState>,
    Json(req): Json<ApplyDiscoveryRequest>,
) -> AppResult<Json<ApplyDiscoveryResponse>> {
    let db = &state.db;
    let txn = db.begin().await?;

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
        let result = process_action(&txn, &action).await;
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

    txn.commit().await?;

    // Trigger aggregate refresh if any readings were backfilled
    if resp.total_backfilled > 0 {
        let db_clone = db.clone();
        tokio::spawn(async move {
            crate::services::sync_state::refresh_continuous_aggregates_full(&db_clone).await;
        });
    }

    Ok(Json(resp))
}

/// Resolve an existing entity or create one by name (case-insensitive match).
async fn resolve_or_create_project<C: ConnectionTrait>(
    db: &C,
    use_id: Option<Uuid>,
    create: Option<&CreateProjectAction>,
) -> Result<(Uuid, bool), String> {
    if let Some(pid) = use_id {
        return Ok((pid, false));
    }
    let cp = create.ok_or("No project specified")?;
    let existing = projects::Entity::find()
        .filter(Expr::cust_with_values("LOWER(name) = $1", [cp.name.to_lowercase()]))
        .one(db).await.map_err(|e| e.to_string())?;
    if let Some(existing) = existing {
        return Ok((existing.id, false));
    }
    let p = projects::ActiveModel {
        id: Set(Uuid::new_v4()),
        name: Set(cp.name.clone()),
        description: Set(None),
        data_source: Set(Some("vaisala".to_string())),
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
    Ok((inserted.id, true))
}

async fn resolve_or_create_site<C: ConnectionTrait>(
    db: &C,
    use_id: Option<Uuid>,
    create: Option<&CreateSiteAction>,
    project_id: Uuid,
) -> Result<(Uuid, bool), String> {
    if let Some(sid) = use_id {
        return Ok((sid, false));
    }
    let cs = create.ok_or("No site specified")?;
    let existing = sites::Entity::find()
        .filter(Expr::cust_with_values("LOWER(name) = $1", [cs.name.to_lowercase()]))
        .one(db).await.map_err(|e| e.to_string())?;
    if let Some(existing) = existing {
        return Ok((existing.id, false));
    }
    let s = sites::ActiveModel {
        id: Set(Uuid::new_v4()),
        project_id: Set(Some(project_id)),
        name: Set(cs.name.clone()),
        latitude: Set(None), longitude: Set(None), altitude_m: Set(None),
        public_slug: Set(None),
        created_at: Set(Some(Utc::now())),
        discovered_at: Set(Some(Utc::now())),
    };
    let inserted = s.insert(db).await.map_err(|e| e.to_string())?;
    Ok((inserted.id, true))
}

async fn resolve_or_create_parameter<C: ConnectionTrait>(
    db: &C,
    use_id: Option<Uuid>,
    create: Option<&CreateParameterAction>,
) -> Result<(Uuid, bool), String> {
    if let Some(pid) = use_id {
        return Ok((pid, false));
    }
    let cp = create.ok_or("No parameter specified")?;
    let existing = parameters::Entity::find()
        .filter(Expr::cust_with_values("LOWER(name) = $1", [cp.name.to_lowercase()]))
        .one(db).await.map_err(|e| e.to_string())?;
    if let Some(existing) = existing {
        return Ok((existing.id, false));
    }
    let display_name = if cp.display_name.is_empty() { cp.name.clone() } else { cp.display_name.clone() };
    let p = parameters::ActiveModel {
        id: Set(Uuid::new_v4()),
        name: Set(cp.name.clone()),
        display_name: Set(display_name),
        default_units: Set(cp.default_units.clone()),
        category: Set(cp.category.clone()),
        data_type: Set("numeric".to_string()),
        description: Set(None),
        aliases: Set(vec![]),
        default_warning_min: Set(None), default_warning_max: Set(None),
        default_alarm_min: Set(None), default_alarm_max: Set(None),
        created_at: Set(Some(Utc::now())),
    };
    let inserted = p.insert(db).await.map_err(|e| e.to_string())?;
    Ok((inserted.id, true))
}

async fn resolve_or_create_site_parameter<C: ConnectionTrait>(
    db: &C,
    pair_to: &str,
    create: Option<&CreateSiteParameterAction>,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Result<(Uuid, bool), String> {
    if pair_to != "new" {
        let id = Uuid::parse_str(pair_to).map_err(|_| "Invalid site_parameter_id".to_string())?;
        return Ok((id, false));
    }
    let existing = site_parameters::Entity::find()
        .filter(Condition::all()
            .add(site_parameters::Column::SiteId.eq(site_id))
            .add(site_parameters::Column::ParameterId.eq(parameter_id)))
        .one(db).await.map_err(|e| e.to_string())?;
    if let Some(existing) = existing {
        return Ok((existing.id, false));
    }
    let param = parameters::Entity::find_by_id(parameter_id)
        .one(db).await.map_err(|e| e.to_string())?
        .ok_or("Parameter not found")?;
    let sp = site_parameters::ActiveModel {
        id: Set(Uuid::new_v4()),
        site_id: Set(site_id),
        parameter_id: Set(parameter_id),
        name: Set(param.name),
        sensor_type: Set(String::new()),
        display_units: Set(create.and_then(|c| c.display_units.clone())),
        units_name: Set(None), units_min: Set(None), units_max: Set(None),
        decimal_places: Set(None),
        channel_id: Set(create.and_then(|c| c.channel_id)),
        sample_interval_sec: Set(create.and_then(|c| c.sample_interval_sec)),
        is_active: Set(Some(true)),
        is_derived: Set(Some(false)),
        derived_definition_id: Set(None),
        variable_mappings: Set(None),
        created_at: Set(Some(Utc::now())),
        updated_at: Set(Some(Utc::now())),
        discovered_at: Set(Some(Utc::now())),
    };
    let inserted = sp.insert(db).await.map_err(|e| e.to_string())?;
    Ok((inserted.id, true))
}

/// Pair a stream to a site_parameter, create sensor, and backfill readings/status_events.
async fn pair_and_backfill<C: ConnectionTrait>(
    db: &C,
    stream_id: Uuid,
    site_parameter_id: Uuid,
) -> Result<(u32, u64), String> {
    let stream = data_streams::Entity::find_by_id(stream_id)
        .one(db).await.map_err(|e| e.to_string())?
        .ok_or("Stream not found")?;
    if stream.site_parameter_id.is_some() {
        return Err("Stream is already paired".to_string());
    }
    let sp = site_parameters::Entity::find_by_id(site_parameter_id)
        .one(db).await.map_err(|e| e.to_string())?
        .ok_or("Site parameter not found")?;

    let sensors_created = if stream.sensor_id.is_none() { 1u32 } else { 0 };
    let sensor_ctx = create_sensor_for_stream(db, &stream, sp.parameter_id, sp.site_id)
        .await.map_err(|e| e.to_string())?;

    // Re-fetch stream (sensor_id may have been updated by create_sensor_for_stream)
    let stream = data_streams::Entity::find_by_id(stream_id)
        .one(db).await.map_err(|e| e.to_string())?
        .ok_or("Stream not found after sensor creation")?;

    let now = Utc::now();
    let mut active: data_streams::ActiveModel = stream.into();
    active.site_parameter_id = Set(Some(site_parameter_id));
    active.paired_at = Set(Some(now.into()));
    active.updated_at = Set(now.into());
    active.update(db).await.map_err(|e| e.to_string())?;

    use sea_orm::Statement;
    let result = db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE readings
          SET site_id = $1, parameter_id = $2,
              sensor_id = $4, calibration_id = $5, deployment_id = $6,
              calibrated_value = COALESCE(calibrated_value, raw_value)
          WHERE stream_id = $3 AND site_id IS NULL",
        [sp.site_id.into(), sp.parameter_id.into(), stream_id.into(),
         sensor_ctx.sensor_id.into(), sensor_ctx.calibration_id.into(), sensor_ctx.deployment_id.into()],
    )).await.map_err(|e| e.to_string())?;
    let backfilled = result.rows_affected();

    let _ = db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE status_events
          SET site_id = $1, parameter_id = $2, sensor_id = $4
          WHERE stream_id = $3 AND site_id IS NULL",
        [sp.site_id.into(), sp.parameter_id.into(), stream_id.into(), sensor_ctx.sensor_id.into()],
    )).await;

    Ok((sensors_created, backfilled))
}

/// Process a single apply-discovery action using extracted helpers.
async fn process_action<C: ConnectionTrait>(
    db: &C,
    action: &ApplyAction,
) -> Result<ActionStats, String> {
    let (project_id, proj_new) = resolve_or_create_project(db, action.use_project_id, action.create_project.as_ref()).await?;
    let (site_id, site_new) = resolve_or_create_site(db, action.use_site_id, action.create_site.as_ref(), project_id).await?;
    let (parameter_id, param_new) = resolve_or_create_parameter(db, action.use_parameter_id, action.create_parameter.as_ref()).await?;
    let (site_parameter_id, sp_new) = resolve_or_create_site_parameter(
        db, &action.pair_to, action.create_site_parameter.as_ref(), site_id, parameter_id,
    ).await?;
    let (sensors_created, backfilled) = pair_and_backfill(db, action.stream_id, site_parameter_id).await?;

    Ok(ActionStats {
        projects_created: u32::from(proj_new),
        sites_created: u32::from(site_new),
        parameters_created: u32::from(param_new),
        site_parameters_created: u32::from(sp_new),
        sensors_created,
        streams_paired: 1,
        backfilled,
    })
}

// ============================================================================
// Grouped Discovery + Bulk Pair
// ============================================================================

#[derive(Deserialize)]
struct GroupedDiscoveryRequest {
    source_system: String,
}

#[derive(Serialize)]
struct GroupedDiscoveryResponse {
    source_system: String,
    total_streams: usize,
    projects: Vec<GroupedProject>,
    sites: Vec<GroupedSite>,
    parameters: Vec<GroupedParameter>,
}

#[derive(Serialize)]
struct GroupedProject {
    name: String,
    stream_count: usize,
    existing_id: Option<Uuid>,
}

#[derive(Serialize)]
struct GroupedSite {
    name: String,
    glacier: Option<String>,
    stream_count: usize,
    existing_id: Option<Uuid>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    altitude_m: Option<f64>,
}

#[derive(Serialize)]
struct GroupedParameter {
    name: String,
    display_name: String,
    units: String,
    stream_count: usize,
    existing_id: Option<Uuid>,
}

/// `POST /api/admin/sync/grouped-discovery` — server-side grouping of unpaired streams.
async fn grouped_discovery(
    State(state): State<AppState>,
    Json(req): Json<GroupedDiscoveryRequest>,
) -> AppResult<Json<GroupedDiscoveryResponse>> {
    let db = &state.db;

    let streams = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq(&req.source_system))
        .filter(data_streams::Column::SiteParameterId.is_null())
        .all(db)
        .await?;

    let total_streams = streams.len();

    // Group by project (from source_path segment 1 or hierarchy metadata)
    let mut project_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    // Group by site (from source_path segment 3) → (glacier_name, count, lat, lon, alt)
    let mut site_info: std::collections::HashMap<String, (Option<String>, usize, Option<f64>, Option<f64>, Option<f64>)> = std::collections::HashMap::new();
    // Group by parameter (from source_name display name, extract the part after " - ")
    let mut param_info: std::collections::HashMap<String, (String, usize)> = std::collections::HashMap::new();

    for stream in &streams {
        // Project: use hierarchy metadata or capitalize source_system
        let project_name = stream.metadata.get("hierarchy")
            .and_then(|h| h.get("project"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| req.source_system.to_uppercase());
        *project_counts.entry(project_name).or_default() += 1;

        // Site: extract from source_path segment 3 (e.g., "nomis/GL1/GL1_DN/..." → "GL1_DN")
        let site_name = stream.source_path.as_deref()
            .and_then(|p| p.split('/').nth(2))
            .unwrap_or("")
            .to_string();
        let glacier_name = stream.metadata.get("glacier")
            .and_then(|g| g.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim_matches('"').to_string());
        if !site_name.is_empty() {
            let coords = stream.metadata.get("coordinates");
            let lat = coords.and_then(|c| c.get("latitude")).and_then(|v| v.as_f64());
            let lon = coords.and_then(|c| c.get("longitude")).and_then(|v| v.as_f64());
            let alt = coords.and_then(|c| c.get("altitude_m")).and_then(|v| v.as_f64());
            let entry = site_info.entry(site_name).or_insert((glacier_name.clone(), 0, lat, lon, alt));
            entry.1 += 1;
        }

        // Parameter: extract display name from source_name (e.g., "GL1_DN - Conductivity" → "Conductivity")
        let param_display = stream.source_name.as_deref()
            .and_then(|n| n.split(" - ").nth(1))
            .unwrap_or("")
            .to_string();
        let units = stream.metadata.get("units")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !param_display.is_empty() {
            let entry = param_info.entry(param_display.clone()).or_insert((units, 0));
            entry.1 += 1;
        }
    }

    // Match against existing entities
    let existing_projects: Vec<(Uuid, String)> = projects::Entity::find()
        .all(db).await?
        .into_iter().map(|p| (p.id, p.name.to_lowercase())).collect();
    let existing_sites: Vec<(Uuid, String)> = sites::Entity::find()
        .all(db).await?
        .into_iter().map(|s| (s.id, s.name.to_lowercase())).collect();
    let existing_params: Vec<(Uuid, String, String)> = parameters::Entity::find()
        .all(db).await?
        .into_iter().map(|p| (p.id, p.name.to_lowercase(), p.display_name)).collect();

    let grouped_projects: Vec<GroupedProject> = project_counts.into_iter()
        .map(|(name, count)| {
            let existing_id = existing_projects.iter()
                .find(|(_, n)| *n == name.to_lowercase())
                .map(|(id, _)| *id);
            GroupedProject { name, stream_count: count, existing_id }
        })
        .collect();

    let mut grouped_sites: Vec<GroupedSite> = site_info.into_iter()
        .map(|(name, (glacier, count, lat, lon, alt))| {
            let existing_id = existing_sites.iter()
                .find(|(_, n)| *n == name.to_lowercase())
                .map(|(id, _)| *id);
            GroupedSite { name, glacier, stream_count: count, existing_id, latitude: lat, longitude: lon, altitude_m: alt }
        })
        .collect();
    grouped_sites.sort_by(|a, b| a.name.cmp(&b.name));

    let mut grouped_params: Vec<GroupedParameter> = param_info.into_iter()
        .map(|(display_name, (units, count))| {
            let existing_id = existing_params.iter()
                .find(|(_, n, _)| *n == display_name.to_lowercase())
                .map(|(id, _, _)| *id);
            GroupedParameter {
                name: display_name.clone(),
                display_name,
                units,
                stream_count: count,
                existing_id,
            }
        })
        .collect();
    grouped_params.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Json(GroupedDiscoveryResponse {
        source_system: req.source_system,
        total_streams,
        projects: grouped_projects,
        sites: grouped_sites,
        parameters: grouped_params,
    }))
}

// ── Bulk Pair ──

#[derive(Deserialize)]
struct BulkPairRequest {
    source_system: String,
    project_name: String,
    /// Sites to create or use. Each has name + optional existing_id.
    sites: Vec<BulkPairSite>,
    /// Parameters to create or use. Each has name, display_name, units + optional existing_id.
    parameters: Vec<BulkPairParameter>,
}

#[derive(Deserialize)]
struct BulkPairSite {
    name: String,
    existing_id: Option<Uuid>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    altitude_m: Option<f64>,
}

#[derive(Deserialize)]
struct BulkPairParameter {
    name: String,
    display_name: String,
    units: String,
    existing_id: Option<Uuid>,
}

#[derive(Serialize)]
struct BulkPairResponse {
    project_created: bool,
    sites_created: u32,
    parameters_created: u32,
    site_parameters_created: u32,
    streams_paired: u32,
}

/// `POST /api/admin/sync/bulk-pair` — creates entities and pairs all matching streams in one transaction.
async fn bulk_pair(
    State(state): State<AppState>,
    Json(req): Json<BulkPairRequest>,
) -> AppResult<Json<BulkPairResponse>> {
    use std::collections::HashMap;

    let db = &state.db;
    let txn = db.begin().await?;

    // 1. Resolve or create project
    let project_created;
    let project_id = {
        let existing = projects::Entity::find()
            .filter(Expr::cust_with_values("LOWER(name) = $1", [req.project_name.to_lowercase()]))
            .one(&txn).await?;
        if let Some(p) = existing {
            project_created = false;
            p.id
        } else {
            let id = Uuid::new_v4();
            projects::ActiveModel {
                id: Set(id),
                name: Set(req.project_name.clone()),
                description: Set(None),
                data_source: Set(Some(req.source_system.clone())),
                is_public: Set(false),
                public_slug: Set(None),
                public_api_title: Set(None),
                public_api_description: Set(None),
                public_api_version: Set(None),
                public_contact_email: Set(None),
                created_at: Set(Some(Utc::now())),
                discovered_at: Set(Some(Utc::now())),
            }.insert(&txn).await?;
            project_created = true;
            id
        }
    };

    // 2. Resolve or create sites → build name→id map
    let mut site_map: HashMap<String, Uuid> = HashMap::new();
    let mut sites_created = 0u32;
    for s in &req.sites {
        let id = if let Some(eid) = s.existing_id {
            eid
        } else {
            // Check if already exists (case-insensitive)
            let existing = sites::Entity::find()
                .filter(Expr::cust_with_values("LOWER(name) = $1", [s.name.to_lowercase()]))
                .one(&txn).await?;
            if let Some(existing) = existing {
                existing.id
            } else {
                let id = Uuid::new_v4();
                sites::ActiveModel {
                    id: Set(id),
                    project_id: Set(Some(project_id)),
                    name: Set(s.name.clone()),
                    latitude: Set(s.latitude), longitude: Set(s.longitude), altitude_m: Set(s.altitude_m),
                    public_slug: Set(None),
                    created_at: Set(Some(Utc::now())),
                    discovered_at: Set(Some(Utc::now())),
                }.insert(&txn).await?;
                sites_created += 1;
                id
            }
        };
        site_map.insert(s.name.to_lowercase(), id);
    }

    // 3. Resolve or create parameters → build name→id map
    let mut param_map: HashMap<String, Uuid> = HashMap::new();
    let mut params_created = 0u32;
    for p in &req.parameters {
        let id = if let Some(eid) = p.existing_id {
            eid
        } else {
            let existing = parameters::Entity::find()
                .filter(Expr::cust_with_values("LOWER(name) = $1", [p.name.to_lowercase()]))
                .one(&txn).await?;
            if let Some(existing) = existing {
                existing.id
            } else {
                let id = Uuid::new_v4();
                parameters::ActiveModel {
                    id: Set(id),
                    name: Set(p.name.clone()),
                    display_name: Set(p.display_name.clone()),
                    default_units: Set(p.units.clone()),
                    category: Set("measurement".to_string()),
                    data_type: Set("numeric".to_string()),
                    description: Set(None),
                    aliases: Set(vec![]),
                    default_warning_min: Set(None), default_warning_max: Set(None),
                    default_alarm_min: Set(None), default_alarm_max: Set(None),
                    created_at: Set(Some(Utc::now())),
                }.insert(&txn).await?;
                params_created += 1;
                id
            }
        };
        param_map.insert(p.name.to_lowercase(), id);
    }

    // 4. Fetch unpaired streams, build site_parameter mappings, then batch-pair
    use sea_orm::Statement;

    let streams = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq(&req.source_system))
        .filter(data_streams::Column::SiteParameterId.is_null())
        .all(&txn)
        .await?;

    // Build param name→id lookup for parameter display names
    let param_name_lookup: HashMap<Uuid, String> = {
        let all = parameters::Entity::find().all(&txn).await?;
        all.into_iter().map(|p| (p.id, p.name)).collect()
    };

    // First pass: determine unique (site_id, parameter_id) pairs and create site_parameters
    let mut sp_cache: HashMap<(Uuid, Uuid), Uuid> = HashMap::new();
    let mut sp_created = 0u32;
    let mut stream_to_sp: Vec<(Uuid, Uuid)> = Vec::with_capacity(streams.len()); // stream_id → site_parameter_id

    for stream in &streams {
        let site_name = stream.source_path.as_deref()
            .and_then(|p| p.split('/').nth(2))
            .unwrap_or("")
            .to_lowercase();
        let param_name = stream.source_name.as_deref()
            .and_then(|n| n.split(" - ").nth(1))
            .unwrap_or("")
            .to_lowercase();

        let Some(&site_id) = site_map.get(&site_name) else {
            stream_to_sp.push((stream.id, Uuid::nil()));
            continue;
        };
        let Some(&parameter_id) = param_map.get(&param_name) else {
            stream_to_sp.push((stream.id, Uuid::nil()));
            continue;
        };

        let sp_key = (site_id, parameter_id);
        let site_parameter_id = if let Some(&sp_id) = sp_cache.get(&sp_key) {
            sp_id
        } else {
            let id = Uuid::new_v4();
            let param_name_val = param_name_lookup.get(&parameter_id).cloned().unwrap_or_default();
            site_parameters::ActiveModel {
                id: Set(id),
                site_id: Set(site_id),
                parameter_id: Set(parameter_id),
                name: Set(param_name_val),
                sensor_type: Set(String::new()),
                display_units: Set(None), units_name: Set(None),
                units_min: Set(None), units_max: Set(None),
                decimal_places: Set(None), channel_id: Set(None),
                sample_interval_sec: Set(None),
                is_active: Set(Some(true)),
                is_derived: Set(Some(false)),
                derived_definition_id: Set(None),
                variable_mappings: Set(None),
                created_at: Set(Some(Utc::now())),
                updated_at: Set(Some(Utc::now())),
                discovered_at: Set(Some(Utc::now())),
            }.insert(&txn).await?;
            sp_created += 1;
            sp_cache.insert(sp_key, id);
            id
        };

        stream_to_sp.push((stream.id, site_parameter_id));
    }

    // Second pass: batch-pair streams using a single UPDATE with a VALUES join
    // Process in chunks to avoid oversized SQL
    let valid_pairs: Vec<(Uuid, Uuid)> = stream_to_sp.into_iter()
        .filter(|(_, sp_id)| !sp_id.is_nil())
        .collect();

    let paired = valid_pairs.len() as u32;
    let now = Utc::now();

    for chunk in valid_pairs.chunks(1000) {
        // Build VALUES clause: ($1, $2), ($3, $4), ...
        let mut values_parts: Vec<String> = Vec::new();
        let mut params: Vec<sea_orm::Value> = Vec::new();
        for (i, (stream_id, sp_id)) in chunk.iter().enumerate() {
            let base = i * 2 + 1;
            values_parts.push(format!("(${}, ${})", base, base + 1));
            params.push((*stream_id).into());
            params.push((*sp_id).into());
        }

        let values_sql = values_parts.join(",");
        let now_param_idx = chunk.len() * 2 + 1;
        params.push(now.into());

        // Batch update data_streams
        let sql = format!(
            "UPDATE data_streams SET site_parameter_id = v.sp_id, paired_at = ${now_param_idx}, updated_at = ${now_param_idx} \
             FROM (VALUES {values_sql}) AS v(stream_id, sp_id) \
             WHERE data_streams.id = v.stream_id::uuid"
        );
        txn.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres, &sql, params,
        )).await?;
    }

    // Raise TimescaleDB decompression limit for the bulk backfill
    txn.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0".to_owned(),
    )).await?;

    // Third pass: batch-backfill readings using a single JOIN update
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE readings r
          SET site_id = sp.site_id, parameter_id = sp.parameter_id,
              calibrated_value = COALESCE(r.calibrated_value, r.raw_value)
          FROM data_streams ds
          JOIN site_parameters sp ON ds.site_parameter_id = sp.id
          WHERE r.stream_id = ds.id AND r.site_id IS NULL
            AND ds.source_system = $1",
        [req.source_system.clone().into()],
    )).await?;

    // Backfill status_events too
    let _ = txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE status_events se
          SET site_id = sp.site_id, parameter_id = sp.parameter_id
          FROM data_streams ds
          JOIN site_parameters sp ON ds.site_parameter_id = sp.id
          WHERE se.stream_id = ds.id AND se.site_id IS NULL
            AND ds.source_system = $1",
        [req.source_system.clone().into()],
    )).await;

    txn.commit().await?;

    // Trigger aggregate refresh in background
    if paired > 0 {
        let db_clone = db.clone();
        tokio::spawn(async move {
            crate::services::sync_state::refresh_continuous_aggregates_full(&db_clone).await;
        });
    }

    tracing::info!(
        source_system = %req.source_system,
        project_created,
        sites_created,
        params_created,
        sp_created,
        paired,
        "Bulk pair complete"
    );

    Ok(Json(BulkPairResponse {
        project_created,
        sites_created,
        parameters_created: params_created,
        site_parameters_created: sp_created,
        streams_paired: paired,
    }))
}

// ============================================================================
// Pairing Plans (Unified Workflow)
// ============================================================================

#[derive(Deserialize)]
struct CreatePairingPlanRequest {
    source_system: String,
}

async fn create_pairing_plan(
    State(state): State<AppState>,
    Json(req): Json<CreatePairingPlanRequest>,
) -> AppResult<Json<crate::entity::pairing_plans::Model>> {
    let plan = crate::services::pairing::create_plan(&state.db, &req.source_system).await?;
    Ok(Json(plan))
}

async fn list_pairing_plans(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<crate::entity::pairing_plans::Model>>> {
    use sea_orm::QueryOrder;
    let plans = crate::entity::pairing_plans::Entity::find()
        .order_by_desc(crate::entity::pairing_plans::Column::CreatedAt)
        .all(&state.db)
        .await?;
    Ok(Json(plans))
}

async fn get_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<crate::entity::pairing_plans::Model>> {
    let plan = crate::entity::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    Ok(Json(plan))
}

#[derive(Deserialize)]
struct UpdatePairingPlanRequest {
    updates: Vec<PlanEntryUpdate>,
}

#[derive(Deserialize)]
struct PlanEntryUpdate {
    stream_id: Uuid,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    project_name: Option<String>,
    #[serde(default)]
    site_name: Option<String>,
    #[serde(default)]
    parameter_name: Option<String>,
    #[serde(default)]
    parameter_units: Option<String>,
}

async fn update_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdatePairingPlanRequest>,
) -> AppResult<Json<crate::entity::pairing_plans::Model>> {
    let plan = crate::entity::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    if plan.status != "draft" {
        return Err(AppError::BadRequest("Can only edit draft plans".to_string()));
    }

    let mut entries: Vec<crate::services::pairing::PlanEntry> =
        serde_json::from_value(plan.entries.clone())
            .map_err(|e| AppError::Internal(format!("Failed to parse entries: {e}")))?;

    for update in &req.updates {
        if let Some(entry) = entries.iter_mut().find(|e| e.stream_id == update.stream_id) {
            if let Some(ref action) = update.action {
                entry.action = action.clone();
            }
            if let Some(ref name) = update.project_name {
                entry.project.name = name.clone();
                entry.project.id = None;
                entry.project.create = true;
            }
            if let Some(ref name) = update.site_name {
                entry.site.name = name.clone();
                entry.site.id = None;
                entry.site.create = true;
            }
            if let Some(ref name) = update.parameter_name {
                entry.parameter.name = name.clone();
                entry.parameter.id = None;
                entry.parameter.create = true;
            }
            if let Some(ref units) = update.parameter_units {
                entry.parameter.units = units.clone();
            }
        }
    }

    let summary = serde_json::to_value(crate::services::pairing::compute_summary_pub(&entries))
        .unwrap_or_default();

    let mut active: crate::entity::pairing_plans::ActiveModel = plan.into();
    active.entries = Set(serde_json::to_value(&entries).unwrap_or_default());
    active.summary = Set(summary);
    let updated = active.update(&state.db).await?;

    Ok(Json(updated))
}

async fn apply_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<crate::services::pairing::ApplyResult>> {
    let result = crate::services::pairing::apply_plan(&state.db, id).await?;
    Ok(Json(result))
}

async fn revert_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    let reverted = crate::services::pairing::revert_plan(&state.db, id).await?;
    Ok(Json(serde_json::json!({ "reverted": reverted })))
}

// ============================================================================
// Unpaired Summary
// ============================================================================

async fn unpaired_summary(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<serde_json::Value>>> {
    use sea_orm::{ConnectionTrait, Statement};
    let rows = state.db.query_all(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT source_system, \
                COUNT(*) FILTER (WHERE site_parameter_id IS NULL) as unpaired, \
                COUNT(*) FILTER (WHERE site_parameter_id IS NOT NULL) as paired \
         FROM data_streams GROUP BY source_system ORDER BY source_system".to_owned(),
    )).await?;

    let result: Vec<serde_json::Value> = rows.iter().map(|row| {
        let source_system: String = row.try_get("", "source_system").unwrap_or_default();
        let unpaired: i64 = row.try_get("", "unpaired").unwrap_or(0);
        let paired: i64 = row.try_get("", "paired").unwrap_or(0);
        serde_json::json!({ "source_system": source_system, "unpaired": unpaired, "paired": paired })
    }).collect();

    Ok(Json(result))
}

// ============================================================================
// Plan Site Metadata
// ============================================================================

async fn plan_site_metadata(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<serde_json::Value>>> {
    use sea_orm::Statement;

    let plan = crate::entity::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    let entries: Vec<crate::services::pairing::PlanEntry> =
        serde_json::from_value(plan.entries.clone())
            .map_err(|e| AppError::Internal(format!("Failed to parse entries: {e}")))?;

    let stream_ids: Vec<Uuid> = entries.iter().map(|e| e.stream_id).collect();
    if stream_ids.is_empty() {
        return Ok(Json(vec![]));
    }

    let ids_str: Vec<String> = stream_ids.iter().map(|id| format!("'{id}'")).collect();
    let ids_list = ids_str.join(",");

    let sql = format!(
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
            metadata->'device'->>'logger_serial' as device_serial, \
            metadata->>'channel_id' as channel_id, \
            metadata->>'sample_interval_sec' as sample_interval_sec \
         FROM data_streams WHERE id IN ({ids_list}) \
         ORDER BY metadata->'hierarchy'->>'site'"
    );

    let rows = state.db.query_all(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql,
    )).await?;

    let result: Vec<serde_json::Value> = rows.iter().map(|row| {
        let get = |col: &str| -> Option<String> {
            row.try_get::<Option<String>>("", col).ok().flatten().filter(|s| s != "null" && !s.is_empty())
        };
        let site_name: String = row.try_get("", "site_name").unwrap_or_default();
        serde_json::json!({
            "site_name": site_name,
            "latitude": get("latitude").and_then(|s| s.parse::<f64>().ok()),
            "longitude": get("longitude").and_then(|s| s.parse::<f64>().ok()),
            "altitude_m": get("altitude_m").and_then(|s| s.parse::<f64>().ok()),
            "glacier_name": get("glacier_name"),
            "glacier_rgi": get("glacier_rgi"),
            "location_type": get("location_type"),
            "catchment": get("catchment"),
            "full_name": get("full_name"),
            "elevation": get("elevation").and_then(|s| s.parse::<f64>().ok()),
            "device_serial": get("device_serial"),
            "channel_id": get("channel_id"),
            "sample_interval_sec": get("sample_interval_sec").and_then(|s| s.parse::<i64>().ok()),
        })
    }).collect();

    Ok(Json(result))
}
