use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, EntityTrait, QueryFilter, Set,
    Statement, TransactionTrait, sea_query::Expr,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::sensors::operations::{
    create_sensor_for_stream, extract_vaisala_device_serial,
};
use crate::routes::private::{
    data_streams, parameters, projects, sites, sites::parameters as site_parameters,
};

/// Per-site info: (glacier_name, count, lat, lon, alt).
type SiteInfoMap = std::collections::HashMap<
    String,
    (Option<String>, usize, Option<f64>, Option<f64>, Option<f64>),
>;

use super::operator;
use axum::routing::patch;

/// Sync admin views are split by required authorization so the unified `/api/` router
/// can layer the right middleware per group without route-level overrides.
///
/// Group membership:
/// - `read_routes`: list/get operations, fine for any read_metadata caller.
/// - `write_routes`: operator actions such as issuing sync commands and pairing workflows.
///   Same gate as other entity mutations (Keycloak admin or write_metadata token).
/// - `admin_routes`: credential listing, creation and revoke, these mint full-permission
///   sync session tokens, so they're Keycloak-admin only (no API token can pass). The listing
///   is admin-gated alongside them, matching `sync_service_credentials` CRUD, so a leaked token
///   cannot enumerate which credentials exist.
pub fn read_routes() -> Router<AppState> {
    Router::new()
        .route("/services", get(operator::list_services))
        .route("/services/{id}", get(operator::get_service))
        .route("/commands", get(operator::list_commands))
        .route("/events", get(operator::list_sync_events))
        .route("/discovery", get(get_discovery))
        .route("/pairing-plans", get(list_pairing_plans))
        .route("/pairing-plans/{id}", get(get_pairing_plan))
        .route("/pairing-plans/{id}/site-metadata", get(plan_site_metadata))
        .route("/unpaired-summary", get(unpaired_summary))
}

pub fn write_routes() -> Router<AppState> {
    Router::new()
        .route("/services/{id}/commands", post(operator::issue_command))
        .route("/services/{id}/revoke", post(operator::revoke_service))
        .route("/apply-discovery", post(apply_discovery))
        .route("/grouped-discovery", post(grouped_discovery))
        .route("/bulk-pair", post(bulk_pair))
        .route("/pairing-plans", post(create_pairing_plan))
        .route("/pairing-plans/{id}", patch(update_pairing_plan))
        .route("/pairing-plans/{id}/apply", post(apply_pairing_plan))
        .route("/pairing-plans/{id}/revert", post(revert_pairing_plan))
}

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/credentials",
            get(operator::list_credentials).post(operator::create_credential),
        )
        .route(
            "/credentials/{id}/revoke",
            post(operator::revoke_credential),
        )
}

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
            matched: Some(DiscoveryMatch {
                id: *id,
                name: cname.clone(),
            }),
            confidence: "exact".to_string(),
            suggested_name: None,
            suggested_units: None,
        };
    }
    // Fuzzy: substring containment
    if let Some((id, cname)) = candidates
        .iter()
        .find(|(_, n)| n.to_lowercase().contains(&lower) || lower.contains(&n.to_lowercase()))
    {
        return DiscoverySuggestion {
            matched: Some(DiscoveryMatch {
                id: *id,
                name: cname.clone(),
            }),
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

/// Return a structured discovery report for unpaired streams, with name-match suggestions
/// for project, site, parameter, and site_parameter resolution. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/sync/discovery",
    responses(
        (status = 200, description = "Array of discovery items with match suggestions", body = Object),
    ),
    tag = "sync"
)]
pub async fn get_discovery(State(state): State<AppState>) -> AppResult<Json<Vec<DiscoveryItem>>> {
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

    let site_candidates: Vec<(Uuid, String)> = all_sites
        .iter()
        .map(|(id, name, _)| (*id, name.clone()))
        .collect();

    let mut items = Vec::new();

    for stream in streams {
        let h = super::service::extract_hierarchy(&stream);
        let (project_name, site_name, param_name) = (h.project, h.site, h.parameter);

        let units = stream
            .metadata
            .get("units")
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
                all_sites
                    .iter()
                    .filter(|(_, _, proj)| *proj == Some(pid))
                    .map(|(id, name, _)| (*id, name.clone()))
                    .collect()
            } else {
                vec![]
            };

            if !site_within_project.is_empty() {
                match_confidence(&site_name, &site_within_project)
            } else {
                match_confidence(&site_name, &site_candidates)
            }
        };

        // Match parameter
        let mut param_suggestion = if param_name.is_empty() {
            DiscoverySuggestion {
                matched: None,
                confidence: "none".to_string(),
                suggested_name: Some(stream.source_name.clone().unwrap_or_default()),
                suggested_units: if units.is_empty() {
                    None
                } else {
                    Some(units.clone())
                },
            }
        } else {
            let mut s = match_confidence(&param_name, &all_params);
            if s.confidence == "none" {
                s.suggested_units = if units.is_empty() {
                    None
                } else {
                    Some(units.clone())
                };
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
            if let Some(sp) = all_site_params
                .iter()
                .find(|(_, sid, pid)| *sid == site_match.id && *pid == param_match.id)
            {
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
        } else if project_suggestion.confidence != "none" && site_suggestion.confidence != "none" {
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
    pub code: String,
    #[serde(default = "default_display_name")]
    pub name: String,
    #[serde(default)]
    pub default_units: String,
    #[serde(default = "default_category")]
    pub category: String,
}

fn default_display_name() -> String {
    String::new()
}
fn default_category() -> String {
    "measurement".to_string()
}

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
    /// The `(site, parameter)` slot the stream landed in, so the caller can re-derive its readings
    /// by window once the pairing transaction has committed.
    slot: (Uuid, Uuid),
}

/// `POST /api/admin/sync/apply-discovery`, batch-processes discovery actions.
///
/// Runs all actions within a single database transaction so that partial failures
/// don't leave orphaned entities or inconsistent pairing state.
/// Apply a discovery decision: create missing project/site/parameter/site_parameter rows,
/// then pair the stream and backfill its readings. Used by the UI's pairing wizard.
/// Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/sync/apply-discovery",
    request_body(content = Object, description = "Discovery actions to apply (per-stream)"),
    responses(
        (status = 200, description = "Pairing results per stream", body = Object),
        (status = 400, description = "Invalid action payload"),
    ),
    tag = "sync"
)]
pub async fn apply_discovery(
    State(state): State<AppState>,
    Json(req): Json<ApplyDiscoveryRequest>,
) -> AppResult<Json<ApplyDiscoveryResponse>> {
    let db = &state.db;
    let txn = db.begin().await?;
    txn.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0".to_owned(),
    ))
    .await?;

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

    let mut backfilled_slots: std::collections::HashSet<(Uuid, Uuid)> =
        std::collections::HashSet::new();

    for action in req.actions {
        // Each action runs in a savepoint so a failure is reported without aborting the rest.
        let savepoint = txn.begin().await?;
        let result = process_action(&savepoint, &action).await;
        match result {
            Ok(stats) => {
                savepoint.commit().await?;
                resp.projects_created += stats.projects_created;
                resp.sites_created += stats.sites_created;
                resp.parameters_created += stats.parameters_created;
                resp.site_parameters_created += stats.site_parameters_created;
                resp.sensors_created += stats.sensors_created;
                resp.streams_paired += stats.streams_paired;
                resp.total_backfilled += stats.backfilled;
                if stats.backfilled > 0 {
                    backfilled_slots.insert(stats.slot);
                }
            }
            Err(e) => {
                savepoint.rollback().await?;
                resp.errors
                    .push(format!("Stream {}: {}", action.stream_id, e));
            }
        }
    }

    txn.commit().await?;

    enqueue_slot_reprocess(db, &backfilled_slots).await?;

    // Refresh aggregates as a tracked job so a failure is visible and rerunnable
    if resp.total_backfilled > 0 {
        crate::routes::private::reprocessing_jobs::worker::enqueue(
            db,
            "refresh_aggregates_full",
            None,
            None,
            &serde_json::json!({ "full": true }),
            None,
        )
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    }

    Ok(Json(resp))
}

/// Re-derive each newly paired `(site, parameter)` slot's readings by window, as a tracked job per
/// slot.
///
/// The pairing UPDATE attributes readings but corrects none of them: which curve covers a reading is
/// a per-reading question its time answers, and the reprocess engine is what asks it (the same one
/// `POST /streams/{id}/pair` and the pairing plans run). Post-commit, because it opens its own
/// transaction and refreshes continuous aggregates, neither of which can run inside the caller's.
async fn enqueue_slot_reprocess(
    db: &sea_orm::DatabaseConnection,
    slots: &std::collections::HashSet<(Uuid, Uuid)>,
) -> AppResult<()> {
    for (site_id, parameter_id) in slots {
        crate::routes::private::reprocessing_jobs::worker::enqueue(
            db,
            "pairing_backfill",
            None,
            None,
            &serde_json::json!({ "site_id": site_id, "parameter_id": parameter_id }),
            None,
        )
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    }
    Ok(())
}

/// Resolve an existing entity or create one by name (case-insensitive match).
async fn resolve_or_create_project<C: ConnectionTrait>(
    db: &C,
    use_id: Option<Uuid>,
    create: Option<&CreateProjectAction>,
    data_source: Option<&str>,
) -> Result<(Uuid, bool), String> {
    if let Some(pid) = use_id {
        return Ok((pid, false));
    }
    let cp = create.ok_or("No project specified")?;
    let existing = projects::Entity::find()
        .filter(Expr::cust_with_values(
            "LOWER(name) = $1",
            [cp.name.to_lowercase()],
        ))
        .one(db)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(existing) = existing {
        return Ok((existing.id, false));
    }
    let p = projects::ActiveModel {
        id: Set(Uuid::new_v4()),
        name: Set(cp.name.clone()),
        description: Set(None),
        data_source: Set(data_source.map(String::from)),
        is_public: Set(false),
        public_code: Set(None),
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
        .filter(Expr::cust_with_values(
            "LOWER(name) = $1",
            [cs.name.to_lowercase()],
        ))
        .one(db)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(existing) = existing {
        return Ok((existing.id, false));
    }
    let s = sites::ActiveModel {
        id: Set(Uuid::new_v4()),
        project_id: Set(Some(project_id)),
        // Left unset: the sites trigger assigns the project's default subproject.
        subproject_id: sea_orm::ActiveValue::NotSet,
        name: Set(cs.name.clone()),
        latitude: Set(None),
        longitude: Set(None),
        altitude_m: Set(None),
        public_code: Set(None),
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
        .filter(Expr::cust_with_values(
            "LOWER(code) = $1",
            [cp.code.to_lowercase()],
        ))
        .one(db)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(existing) = existing {
        return Ok((existing.id, false));
    }
    let name = if cp.name.is_empty() {
        cp.code.clone()
    } else {
        cp.name.clone()
    };
    let p = parameters::ActiveModel {
        id: Set(Uuid::new_v4()),
        code: Set(cp.code.clone()),
        name: Set(name),
        default_units: Set(cp.default_units.clone()),
        category: Set(cp.category.clone()),
        description: Set(None),
        aliases: Set(vec![]),
        default_warning_min: Set(None),
        default_warning_max: Set(None),
        default_alarm_min: Set(None),
        default_alarm_max: Set(None),
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
        .filter(
            Condition::all()
                .add(site_parameters::Column::SiteId.eq(site_id))
                .add(site_parameters::Column::ParameterId.eq(parameter_id)),
        )
        .one(db)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(existing) = existing {
        return Ok((existing.id, false));
    }
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
        display_units: Set(create.and_then(|c| c.display_units.clone())),
        units_name: Set(None),
        units_min: Set(None),
        units_max: Set(None),
        decimal_places: Set(None),
        channel_id: Set(create.and_then(|c| c.channel_id)),
        sample_interval_sec: Set(create.and_then(|c| c.sample_interval_sec)),
        is_active: Set(Some(true)),
        is_public: Set(Some(false)),
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
///
/// Runs inside the caller's transaction, so the window re-derivation it needs (which opens its own
/// transaction and refreshes continuous aggregates) is left to the caller to enqueue post-commit.
async fn pair_and_backfill<C: ConnectionTrait>(
    db: &C,
    stream_id: Uuid,
    site_parameter_id: Uuid,
) -> Result<(u32, u64), String> {
    let stream = data_streams::Entity::find_by_id(stream_id)
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

    let sensors_created = if stream.sensor_id.is_none() { 1u32 } else { 0 };
    let sensor_ctx = create_sensor_for_stream(db, &stream, sp.parameter_id, sp.site_id)
        .await
        .map_err(|e| e.to_string())?;

    // Re-fetch stream (sensor_id may have been updated by create_sensor_for_stream)
    let stream = data_streams::Entity::find_by_id(stream_id)
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

    // Attribution only: site, parameter, the owning instrument and its deployment. No curve is
    // stamped here. The context carries the sensor's newest calibration, which is neither the curve
    // whose window covers a given reading nor necessarily one authored for this parameter, so
    // claiming it on a whole backfilled history would assert a correction that was never applied.
    // Both callers enqueue a slot reprocess post-commit; that is what resolves `calibration_id` and
    // `calibrated_value` per reading, from the reading's own time.
    let result = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings r
          SET site_id = $1, parameter_id = $2,
              sensor_id = $4, deployment_id = $5,
              measurement_type = COALESCE(r.measurement_type, ds.measurement_type)
          FROM data_streams ds
          WHERE r.stream_id = ds.id AND ds.id = $3 AND r.site_id IS NULL",
            [
                sp.site_id.into(),
                sp.parameter_id.into(),
                stream_id.into(),
                sensor_ctx.sensor_id.into(),
                sensor_ctx.deployment_id.into(),
            ],
        ))
        .await
        .map_err(|e| e.to_string())?;
    let backfilled = result.rows_affected();

    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE status_events
          SET site_id = $1, parameter_id = $2, sensor_id = $4
          WHERE stream_id = $3 AND site_id IS NULL",
        [
            sp.site_id.into(),
            sp.parameter_id.into(),
            stream_id.into(),
            sensor_ctx.sensor_id.into(),
        ],
    ))
    .await
    .map_err(|e| e.to_string())?;

    Ok((sensors_created, backfilled))
}

/// Process a single apply-discovery action using extracted helpers.
async fn process_action<C: ConnectionTrait>(
    db: &C,
    action: &ApplyAction,
) -> Result<ActionStats, String> {
    let stream_source = data_streams::Entity::find_by_id(action.stream_id)
        .one(db)
        .await
        .map_err(|e| e.to_string())?
        .map(|s| s.source_system);
    let (project_id, proj_new) = resolve_or_create_project(
        db,
        action.use_project_id,
        action.create_project.as_ref(),
        stream_source.as_deref(),
    )
    .await?;
    let (site_id, site_new) = resolve_or_create_site(
        db,
        action.use_site_id,
        action.create_site.as_ref(),
        project_id,
    )
    .await?;
    let (parameter_id, param_new) = resolve_or_create_parameter(
        db,
        action.use_parameter_id,
        action.create_parameter.as_ref(),
    )
    .await?;
    let (site_parameter_id, sp_new) = resolve_or_create_site_parameter(
        db,
        &action.pair_to,
        action.create_site_parameter.as_ref(),
        site_id,
        parameter_id,
    )
    .await?;
    let (sensors_created, backfilled) =
        pair_and_backfill(db, action.stream_id, site_parameter_id).await?;

    Ok(ActionStats {
        projects_created: u32::from(proj_new),
        sites_created: u32::from(site_new),
        parameters_created: u32::from(param_new),
        site_parameters_created: u32::from(sp_new),
        sensors_created,
        streams_paired: 1,
        backfilled,
        slot: (site_id, parameter_id),
    })
}

#[derive(Deserialize)]
pub struct GroupedDiscoveryRequest {
    source_system: String,
}

#[derive(Serialize)]
pub struct GroupedDiscoveryResponse {
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
    code: String,
    name: String,
    units: String,
    stream_count: usize,
    existing_id: Option<Uuid>,
}

/// `POST /api/admin/sync/grouped-discovery`, server-side grouping of unpaired streams.
/// Like apply-discovery but groups streams by site for bulk creation. Returns counts
/// of created/paired entities per group. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/sync/grouped-discovery",
    request_body(content = Object, description = "Stream groupings with site-level decisions"),
    responses(
        (status = 200, description = "Per-group counts of created/paired entities", body = Object),
    ),
    tag = "sync"
)]
pub async fn grouped_discovery(
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
    let mut project_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    // Group by site (from source_path segment 3) -> (glacier_name, count, lat, lon, alt)
    let mut site_info: SiteInfoMap = std::collections::HashMap::new();
    // Group by parameter (from source_name display name, extract the part after " - ")
    let mut param_info: std::collections::HashMap<String, (String, usize)> =
        std::collections::HashMap::new();

    for stream in &streams {
        let hierarchy = super::service::extract_hierarchy(stream);
        let project_name = if hierarchy.project.is_empty() {
            req.source_system.to_uppercase()
        } else {
            hierarchy.project.clone()
        };
        *project_counts.entry(project_name).or_default() += 1;

        let site_name = hierarchy.site.clone();
        let glacier_name = stream
            .metadata
            .get("glacier")
            .and_then(|g| g.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim_matches('"').to_string());
        if !site_name.is_empty() {
            let entry = site_info.entry(site_name).or_insert((
                glacier_name.clone(),
                0,
                hierarchy.latitude,
                hierarchy.longitude,
                hierarchy.altitude_m,
            ));
            entry.1 += 1;
        }

        let param_display = hierarchy.parameter.clone();
        let units = stream
            .metadata
            .get("units")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !param_display.is_empty() {
            let entry = param_info
                .entry(param_display.clone())
                .or_insert((units, 0));
            entry.1 += 1;
        }
    }

    // Match against existing entities
    let existing_projects: Vec<(Uuid, String)> = projects::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|p| (p.id, p.name.to_lowercase()))
        .collect();
    let existing_sites: Vec<(Uuid, String)> = sites::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.id, s.name.to_lowercase()))
        .collect();
    // (id, lowercased code, lowercased name, lowercased aliases) so the grouped view
    // matches the same way the pairing plan resolves parameters
    let existing_params: Vec<(Uuid, String, String, Vec<String>)> = parameters::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|p| {
            let aliases = p.aliases.into_iter().map(|a| a.to_lowercase()).collect();
            (p.id, p.code.to_lowercase(), p.name.to_lowercase(), aliases)
        })
        .collect();

    let grouped_projects: Vec<GroupedProject> = project_counts
        .into_iter()
        .map(|(name, count)| {
            let existing_id = existing_projects
                .iter()
                .find(|(_, n)| *n == name.to_lowercase())
                .map(|(id, _)| *id);
            GroupedProject {
                name,
                stream_count: count,
                existing_id,
            }
        })
        .collect();

    let mut grouped_sites: Vec<GroupedSite> = site_info
        .into_iter()
        .map(|(name, (glacier, count, lat, lon, alt))| {
            let existing_id = existing_sites
                .iter()
                .find(|(_, n)| *n == name.to_lowercase())
                .map(|(id, _)| *id);
            GroupedSite {
                name,
                glacier,
                stream_count: count,
                existing_id,
                latitude: lat,
                longitude: lon,
                altitude_m: alt,
            }
        })
        .collect();
    grouped_sites.sort_by(|a, b| a.name.cmp(&b.name));

    let mut grouped_params: Vec<GroupedParameter> = param_info
        .into_iter()
        .map(|(label, (units, count))| {
            let key = label.to_lowercase();
            let existing_id = existing_params
                .iter()
                .find(|(_, code, name, aliases)| {
                    *code == key || *name == key || aliases.contains(&key)
                })
                .map(|(id, ..)| *id);
            GroupedParameter {
                code: label.clone(),
                name: label,
                units,
                stream_count: count,
                existing_id,
            }
        })
        .collect();
    grouped_params.sort_by(|a, b| a.code.cmp(&b.code));

    Ok(Json(GroupedDiscoveryResponse {
        source_system: req.source_system,
        total_streams,
        projects: grouped_projects,
        sites: grouped_sites,
        parameters: grouped_params,
    }))
}

#[derive(Deserialize)]
pub struct BulkPairRequest {
    source_system: String,
    project_name: String,
    /// Sites to create or use. Each has name + optional existing_id.
    sites: Vec<BulkPairSite>,
    /// Parameters to create or use. Each has code, name, units + optional existing_id.
    parameters: Vec<BulkPairParameter>,
}

#[derive(Deserialize)]
pub struct BulkPairSite {
    name: String,
    existing_id: Option<Uuid>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    altitude_m: Option<f64>,
}

#[derive(Deserialize)]
pub struct BulkPairParameter {
    code: String,
    name: String,
    units: String,
    existing_id: Option<Uuid>,
}

#[derive(Serialize)]
pub struct BulkPairResponse {
    project_created: bool,
    sites_created: u32,
    parameters_created: u32,
    site_parameters_created: u32,
    streams_paired: u32,
    /// Streams that could not be paired, with the reason.
    streams_skipped: Vec<String>,
}

/// `POST /api/admin/sync/bulk-pair`, creates entities and pairs all matching streams in one transaction.
/// Bulk-pair multiple streams to existing site_parameters in a single transaction.
/// Backfills readings for each paired stream. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/sync/bulk-pair",
    request_body(content = Object, description = "List of (stream_id, site_parameter_id) pairings"),
    responses(
        (status = 200, description = "Pairing counts and backfill totals", body = Object),
    ),
    tag = "sync"
)]
pub async fn bulk_pair(
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
            .filter(Expr::cust_with_values(
                "LOWER(name) = $1",
                [req.project_name.to_lowercase()],
            ))
            .one(&txn)
            .await?;
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
                public_code: Set(None),
                public_api_title: Set(None),
                public_api_description: Set(None),
                public_api_version: Set(None),
                public_contact_email: Set(None),
                created_at: Set(Some(Utc::now())),
                discovered_at: Set(Some(Utc::now())),
            }
            .insert(&txn)
            .await?;
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
                .filter(Expr::cust_with_values(
                    "LOWER(name) = $1",
                    [s.name.to_lowercase()],
                ))
                .one(&txn)
                .await?;
            if let Some(existing) = existing {
                existing.id
            } else {
                let id = Uuid::new_v4();
                sites::ActiveModel {
                    id: Set(id),
                    project_id: Set(Some(project_id)),
                    subproject_id: sea_orm::ActiveValue::NotSet,
                    name: Set(s.name.clone()),
                    latitude: Set(s.latitude),
                    longitude: Set(s.longitude),
                    altitude_m: Set(s.altitude_m),
                    public_code: Set(None),
                    created_at: Set(Some(Utc::now())),
                    discovered_at: Set(Some(Utc::now())),
                }
                .insert(&txn)
                .await?;
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
                .filter(Expr::cust_with_values(
                    "LOWER(code) = $1",
                    [p.code.to_lowercase()],
                ))
                .one(&txn)
                .await?;
            if let Some(existing) = existing {
                existing.id
            } else {
                let id = Uuid::new_v4();
                parameters::ActiveModel {
                    id: Set(id),
                    code: Set(p.code.clone()),
                    name: Set(p.name.clone()),
                    default_units: Set(p.units.clone()),
                    category: Set("measurement".to_string()),
                    description: Set(None),
                    aliases: Set(vec![]),
                    default_warning_min: Set(None),
                    default_warning_max: Set(None),
                    default_alarm_min: Set(None),
                    default_alarm_max: Set(None),
                    created_at: Set(Some(Utc::now())),
                }
                .insert(&txn)
                .await?;
                params_created += 1;
                id
            }
        };
        param_map.insert(p.code.to_lowercase(), id);
        param_map.insert(p.name.to_lowercase(), id);
    }

    // Same precedence as `lookup_parameter_by_code_name_or_alias`: code wins over display name,
    // which wins over alias. `or_insert` preserves that order as the map is filled.
    for param in parameters::Entity::find().all(&txn).await? {
        param_map
            .entry(param.code.to_lowercase())
            .or_insert(param.id);
        param_map
            .entry(param.name.to_lowercase())
            .or_insert(param.id);
        for alias in param.aliases {
            param_map.entry(alias.to_lowercase()).or_insert(param.id);
        }
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
        let hierarchy = super::service::extract_hierarchy(stream);
        let site_name = hierarchy.site.to_lowercase();
        let param_name = hierarchy.parameter.to_lowercase();

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
            let param_name_val = param_name_lookup
                .get(&parameter_id)
                .cloned()
                .unwrap_or_default();
            site_parameters::ActiveModel {
                id: Set(id),
                site_id: Set(site_id),
                parameter_id: Set(parameter_id),
                name: Set(param_name_val),
                sensor_type: Set(String::new()),
                display_units: Set(None),
                units_name: Set(None),
                units_min: Set(None),
                units_max: Set(None),
                decimal_places: Set(None),
                channel_id: Set(None),
                sample_interval_sec: Set(None),
                is_active: Set(Some(true)),
                is_public: Set(Some(false)),
                is_derived: Set(Some(false)),
                derived_definition_id: Set(None),
                variable_mappings: Set(None),
                created_at: Set(Some(Utc::now())),
                updated_at: Set(Some(Utc::now())),
                discovered_at: Set(Some(Utc::now())),
            }
            .insert(&txn)
            .await?;
            sp_created += 1;
            sp_cache.insert(sp_key, id);
            id
        };

        stream_to_sp.push((stream.id, site_parameter_id));
    }

    // Raise the TimescaleDB decompression limit so backfills reach compressed history
    txn.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0".to_owned(),
    ))
    .await?;

    // Second pass: pair each stream through the same helper the plan flow uses, so sensors,
    // deployment attribution, status events and samples all follow; the curves each reading is
    // corrected by follow from the slot reprocess enqueued after the commit.
    let mut paired = 0u32;
    let mut skipped: Vec<String> = Vec::new();
    let slot_of_sp: HashMap<Uuid, (Uuid, Uuid)> =
        sp_cache.iter().map(|(&slot, &sp)| (sp, slot)).collect();
    let mut backfilled_slots: std::collections::HashSet<(Uuid, Uuid)> =
        std::collections::HashSet::new();
    for (stream_id, sp_id) in stream_to_sp {
        if sp_id.is_nil() {
            skipped.push(stream_id.to_string());
            continue;
        }
        match pair_and_backfill(&txn, stream_id, sp_id).await {
            Ok((_, backfilled)) => {
                paired += 1;
                if let Some(&slot) = slot_of_sp.get(&sp_id).filter(|_| backfilled > 0) {
                    backfilled_slots.insert(slot);
                }
            }
            Err(e) => {
                skipped.push(format!("{stream_id}: {e}"));
            }
        }
    }

    // Every stream considered, not only the paired ones: densification is idempotent per stream
    // and a stream skipped this run may still carry a group that starts above index 0.
    let considered_ids: Vec<Uuid> = streams.iter().map(|s| s.id).collect();
    crate::routes::private::readings::replicates::densify_stream_replicates(&txn, &considered_ids)
        .await?;

    txn.commit().await?;

    enqueue_slot_reprocess(db, &backfilled_slots).await?;

    // Refresh aggregates as a tracked job so a failure is visible and rerunnable
    if paired > 0 {
        crate::routes::private::reprocessing_jobs::worker::enqueue(
            db,
            "refresh_aggregates_full",
            None,
            None,
            &serde_json::json!({ "full": true }),
            None,
        )
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    }

    tracing::info!(
        source_system = %req.source_system,
        project_created,
        sites_created,
        params_created,
        sp_created,
        paired,
        skipped = skipped.len(),
        "Bulk pair complete"
    );

    Ok(Json(BulkPairResponse {
        project_created,
        sites_created,
        parameters_created: params_created,
        site_parameters_created: sp_created,
        streams_paired: paired,
        streams_skipped: skipped,
    }))
}

#[derive(Deserialize)]
pub struct CreatePairingPlanRequest {
    source_system: String,
}

/// Create a draft pairing plan describing a batch of intended stream-to-site_parameter
/// pairings. The plan is reviewable and applied separately. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/sync/pairing-plans",
    request_body(content = Object),
    responses(
        (status = 200, description = "Created pairing plan", body = Object),
    ),
    tag = "sync"
)]
pub async fn create_pairing_plan(
    State(state): State<AppState>,
    Json(req): Json<CreatePairingPlanRequest>,
) -> AppResult<Json<crate::routes::private::data_streams::pairing_plans::Model>> {
    let plan =
        crate::routes::private::sync::service::create_plan(&state.db, &req.source_system).await?;
    Ok(Json(plan))
}

/// List existing pairing plans by status (draft/applied/reverted). Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/sync/pairing-plans",
    responses(
        (status = 200, description = "Array of pairing plans", body = Object),
    ),
    tag = "sync"
)]
pub async fn list_pairing_plans(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<crate::routes::private::data_streams::pairing_plans::Model>>> {
    use sea_orm::QueryOrder;
    let plans = crate::routes::private::data_streams::pairing_plans::Entity::find()
        .order_by_desc(crate::routes::private::data_streams::pairing_plans::Column::CreatedAt)
        .all(&state.db)
        .await?;
    Ok(Json(plans))
}

/// Get a single pairing plan with its full pairing list. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/sync/pairing-plans/{id}",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    responses(
        (status = 200, description = "Pairing plan with intended pairings", body = Object),
        (status = 404, description = "Plan not found"),
    ),
    tag = "sync"
)]
pub async fn get_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<crate::routes::private::data_streams::pairing_plans::Model>> {
    let plan = crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    Ok(Json(plan))
}

#[derive(Deserialize)]
pub struct UpdatePairingPlanRequest {
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

/// Edit a draft pairing plan (only `draft` status allows updates). Requires `write_metadata`.
#[utoipa::path(
    patch,
    path = "/sync/pairing-plans/{id}",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    request_body(content = Object),
    responses(
        (status = 200, description = "Updated plan", body = Object),
        (status = 404, description = "Plan not found"),
        (status = 409, description = "Plan not in draft status"),
    ),
    tag = "sync"
)]
pub async fn update_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdatePairingPlanRequest>,
) -> AppResult<Json<crate::routes::private::data_streams::pairing_plans::Model>> {
    let plan = crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    if plan.status != "draft" {
        return Err(AppError::BadRequest(
            "Can only edit draft plans".to_string(),
        ));
    }

    let mut entries: Vec<crate::routes::private::sync::service::PlanEntry> =
        serde_json::from_value(plan.entries.clone())
            .map_err(|e| AppError::Internal(format!("Failed to parse entries: {e}")))?;

    let catalog = crate::routes::private::sync::service::load_entity_catalog(&state.db).await?;

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
                    .push("site or parameter name is empty".to_string());
            }
        }
    }

    let summary = serde_json::to_value(crate::routes::private::sync::service::compute_summary_pub(
        &entries,
    ))
    .unwrap_or_default();

    let mut active: crate::routes::private::data_streams::pairing_plans::ActiveModel = plan.into();
    active.entries = Set(serde_json::to_value(&entries).unwrap_or_default());
    active.summary = Set(summary);
    let updated = active.update(&state.db).await?;

    Ok(Json(updated))
}

/// Apply a pairing plan: execute all its pairings and backfills atomically. Marks the
/// plan as `applied`. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/sync/pairing-plans/{id}/apply",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    responses(
        (status = 200, description = "Plan applied with execution counts", body = Object),
        (status = 404, description = "Plan not found"),
        (status = 409, description = "Plan already applied or reverted"),
    ),
    tag = "sync"
)]
pub async fn apply_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    // Validate synchronously for immediate feedback, then background the heavy entity-resolution +
    // readings backfill as a tracked job so the request doesn't block. The job's `detail` carries
    // the execution counts the UI used to read from the response.
    let status = plan_status(&state.db, id).await?;
    if status != "draft" {
        return Err(AppError::BadRequest(format!(
            "Plan is '{status}', can only apply 'draft' plans"
        )));
    }
    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        &state.db,
        "plan_apply",
        None,
        Some(id),
        &serde_json::json!({ "plan_id": id }),
        None,
    )
    .await?;
    Ok(Json(
        serde_json::json!({ "job_id": job_id, "status": "queued" }),
    ))
}

/// Fetch a pairing plan's status, or 404 if unknown.
async fn plan_status(db: &sea_orm::DatabaseConnection, id: Uuid) -> AppResult<String> {
    let row = db
        .query_one(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT status FROM pairing_plans WHERE id = $1",
            [id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    Ok(row.try_get::<String>("", "status")?)
}

/// Revert an applied pairing plan: unpair every stream it touched, restoring the prior
/// state. Marks the plan as `reverted`. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/sync/pairing-plans/{id}/revert",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    responses(
        (status = 200, description = "Plan reverted with unpaired counts", body = Object),
        (status = 404, description = "Plan not found"),
        (status = 409, description = "Plan not in applied status"),
    ),
    tag = "sync"
)]
pub async fn revert_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    let status = plan_status(&state.db, id).await?;
    if status != "applied" {
        return Err(AppError::BadRequest(format!(
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
    Ok(Json(
        serde_json::json!({ "job_id": job_id, "status": "queued" }),
    ))
}

/// Aggregate summary of unpaired streams grouped by source system. Used by the dashboard
/// to surface streams needing attention. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/sync/unpaired-summary",
    responses(
        (status = 200, description = "Counts of unpaired streams by source_system", body = Object),
    ),
    tag = "sync"
)]
pub async fn unpaired_summary(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<serde_json::Value>>> {
    use sea_orm::{ConnectionTrait, Statement};
    let rows = state
        .db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT source_system, \
                COUNT(*) FILTER (WHERE site_parameter_id IS NULL) as unpaired, \
                COUNT(*) FILTER (WHERE site_parameter_id IS NOT NULL) as paired \
         FROM data_streams GROUP BY source_system ORDER BY source_system"
                .to_owned(),
        ))
        .await?;

    let result: Vec<serde_json::Value> = rows.iter().map(|row| {
        let source_system: String = row.try_get("", "source_system").unwrap_or_default();
        let unpaired: i64 = row.try_get("", "unpaired").unwrap_or(0);
        let paired: i64 = row.try_get("", "paired").unwrap_or(0);
        serde_json::json!({ "source_system": source_system, "unpaired": unpaired, "paired": paired })
    }).collect();

    Ok(Json(result))
}

/// Get site metadata enrichment for a pairing plan: latitudes, longitudes, glacier names,
/// stream counts. Used by the pairing UI to display context. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/sync/pairing-plans/{id}/site-metadata",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    responses(
        (status = 200, description = "Site metadata map keyed by site name", body = Object),
        (status = 404, description = "Plan not found"),
    ),
    tag = "sync"
)]
pub async fn plan_site_metadata(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<serde_json::Value>>> {
    use sea_orm::Statement;

    let plan = crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    let entries: Vec<crate::routes::private::sync::service::PlanEntry> =
        serde_json::from_value(plan.entries.clone())
            .map_err(|e| AppError::Internal(format!("Failed to parse entries: {e}")))?;

    let stream_ids: Vec<Uuid> = entries.iter().map(|e| e.stream_id).collect();
    if stream_ids.is_empty() {
        return Ok(Json(vec![]));
    }

    let rows = state
        .db
        .query_all(Statement::from_sql_and_values(
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
            metadata->'device'->>'logger_serial' as device_serial, \
            metadata->>'channel_id' as channel_id, \
            metadata->>'sample_interval_sec' as sample_interval_sec \
         FROM data_streams WHERE id = ANY($1) \
         ORDER BY metadata->'hierarchy'->>'site'",
            [sea_orm::Value::Array(
                sea_orm::sea_query::ArrayType::Uuid,
                Some(Box::new(
                    stream_ids
                        .into_iter()
                        .map(|id| sea_orm::Value::Uuid(Some(Box::new(id))))
                        .collect(),
                )),
            )],
        ))
        .await?;

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
