use axum::{
    Json, Router,
    extract::{Path, Query, State},
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
    data_streams, parameters, projects, sensors, sites, sites::parameters as site_parameters,
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
        .route("/services", get(operator::list_services))
        .route("/services/{id}", get(operator::get_service))
        .route("/commands", get(operator::list_commands))
        .route("/commands/{id}", get(operator::get_command))
        .route("/events", get(operator::list_sync_events))
        .route("/discovery", get(get_discovery))
        .route("/pairing-plans", get(list_pairing_plans))
        .route("/pairing-plans/{id}", get(get_pairing_plan))
        .route("/pairing-plans/{id}/site-metadata", get(plan_site_metadata))
        .route("/pairing-plans/{id}/instruments", get(plan_instruments))
        .route("/unpaired-summary", get(unpaired_summary))
}

pub fn write_routes() -> Router<AppState> {
    Router::new()
        .route("/services/{id}", patch(operator::update_service))
        .route("/services/{id}/commands", post(operator::issue_command))
        .route("/services/{id}/revoke", post(operator::revoke_service))
        .route("/apply-discovery", post(apply_discovery))
        .route("/grouped-discovery", post(grouped_discovery))
        .route("/bulk-pair", post(bulk_pair))
        .route("/pairing-plans", post(create_pairing_plan))
        .route("/pairing-plans/{id}", patch(update_pairing_plan))
        .route("/pairing-plans/{id}/apply", post(apply_pairing_plan))
        .route("/pairing-plans/{id}/supersede", post(supersede_pairing_plan))
        .route("/pairing-plans/{id}/revert", post(revert_pairing_plan))
}

pub fn manage_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/replicate_audit_holds",
            get(super::replicate_audit::list_holds),
        )
        .route(
            "/replicate_audit_holds/{id}/acknowledge",
            post(super::replicate_audit::acknowledge_hold),
        )
        .route(
            "/replicate_audit_holds/{id}/resolve",
            post(super::replicate_audit::resolve_hold),
        )
        .route(
            "/replicate_audit_holds/{id}/reopen",
            post(super::replicate_audit::reopen_hold),
        )
        .route(
            "/replicate_audit_holds/acknowledge_bulk",
            post(super::replicate_audit::acknowledge_holds_bulk),
        )
        .route(
            "/replicate_reconciliation/duplicate_slots",
            get(super::replicate_reconciliation::duplicate_slots),
        )
        .route(
            "/replicate_reconciliation/candidates",
            get(super::replicate_reconciliation::reconciliation_candidates),
        )
        .route(
            "/replicate_reconciliation",
            post(super::replicate_reconciliation::start_reconciliation),
        )
}

/// Destructive reconciliation: the delete job removes obsolete streams and their readings, so it
/// sits behind the same gate as stream deletion on CRUD (Keycloak Administrator or a
/// write_metadata token), not the manager review layer the non-destructive endpoints use.
pub fn destructive_routes() -> Router<AppState> {
    Router::new().route(
        "/replicate_reconciliation/delete",
        post(super::replicate_reconciliation::start_reconciliation_delete),
    )
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
    path = "/api/sync/discovery",
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
    path = "/api/sync/apply-discovery",
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
    crate::common::bulk_write::lift_decompression_cap(&txn).await?;

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
        meteoswiss_station_abbr: sea_orm::ActiveValue::NotSet,
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
        // Mechanically created from a sync source; a manager confirms or merges it later.
        needs_review: Set(true),
        description: Set(None),
        aliases: Set(vec![]),
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
        needs_review: Set(false),
        sd_estimator: Set(None),
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
    crate::routes::private::data_streams::service::declare_slot_decimal_places(
        db,
        sp.id,
        crate::routes::private::data_streams::service::declared_decimal_places(&stream.metadata),
    )
    .await
    .map_err(|e| e.to_string())?;

    let sensor_ctx = create_sensor_for_stream(db, &stream, sp.parameter_id, sp.site_id)
        .await
        .map_err(|e| e.to_string())?;
    let sensors_created = u32::from(stream.sensor_id.is_none());
    let sensor_id = Some(sensor_ctx.sensor_id);
    let deployment_id = sensor_ctx.deployment_id;

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
        .execute_raw(Statement::from_sql_and_values(
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
                sensor_id.into(),
                deployment_id.into(),
            ],
        ))
        .await
        .map_err(|e| e.to_string())?;
    let backfilled = result.rows_affected();

    db.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE status_events
          SET site_id = $1, parameter_id = $2, sensor_id = $4
          WHERE stream_id = $3 AND site_id IS NULL",
        [
            sp.site_id.into(),
            sp.parameter_id.into(),
            stream_id.into(),
            sensor_id.into(),
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
    path = "/api/sync/grouped-discovery",
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
    path = "/api/sync/bulk-pair",
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
                    meteoswiss_station_abbr: sea_orm::ActiveValue::NotSet,
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

    // 3. Resolve or create parameters, then build the name→id map over the whole catalog.
    // An incoming parameter without an `existing_id` resolves the way the pairing plan does
    // (`lookup_parameter_by_code_name_or_alias`), so a request naming a catalog row by its display
    // name or alias reuses it rather than minting a sibling.
    let mut catalog = super::service::load_entity_catalog(&txn).await?.params;
    let mut params_created = 0u32;
    // Request parameters bound to a catalog row by id, under the names the request gives them.
    let mut chosen: Vec<(String, Uuid)> = Vec::new();
    for p in &req.parameters {
        if let Some(eid) = p.existing_id {
            chosen.push((p.code.to_lowercase(), eid));
            chosen.push((p.name.to_lowercase(), eid));
            continue;
        }
        if super::service::lookup_parameter_by_code_name_or_alias(&p.code, &catalog).is_some() {
            continue;
        }
        let id = Uuid::new_v4();
        parameters::ActiveModel {
            id: Set(id),
            code: Set(p.code.clone()),
            name: Set(p.name.clone()),
            default_units: Set(p.units.clone()),
            category: Set("measurement".to_string()),
            // Mechanically created from a sync source; a manager confirms or merges it later.
            needs_review: Set(true),
            description: Set(None),
            aliases: Set(vec![]),
            created_at: Set(Some(Utc::now())),
        }
        .insert(&txn)
        .await?;
        params_created += 1;
        catalog.push(super::service::CatalogParam {
            id,
            code: p.code.clone(),
            name: p.name.clone(),
            aliases: vec![],
            units: p.units.clone(),
            category: "measurement".to_string(),
            site_parameter_count: 0,
            reading_count: 0,
        });
    }

    // Same precedence as `lookup_parameter_by_code_name_or_alias`: every code wins over every
    // display name, which wins over every alias. Filled with `or_insert` in that order, so one
    // parameter's name can never shadow another's code; the request's explicit choices go first.
    let mut param_map: HashMap<String, Uuid> = HashMap::new();
    for (name, id) in chosen {
        param_map.entry(name).or_insert(id);
    }
    for param in &catalog {
        param_map
            .entry(param.code.to_lowercase())
            .or_insert(param.id);
    }
    for param in &catalog {
        param_map
            .entry(param.name.to_lowercase())
            .or_insert(param.id);
    }
    for param in &catalog {
        for alias in &param.aliases {
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
                needs_review: Set(false),
                sd_estimator: Set(None),
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

    // The pairing backfills below reach compressed history.
    crate::common::bulk_write::lift_decompression_cap(&txn).await?;

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
    path = "/api/sync/pairing-plans",
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

#[derive(Deserialize)]
pub struct ListPairingPlansQuery {
    #[serde(default)]
    source_system: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

/// A plan without its `entries` document. A CNET draft's entries are 185 kB and a NOMIS draft's
/// 2.5 MB, and the listing is a way back into a review, not a way to read every draft at once.
#[derive(Serialize)]
pub struct PairingPlanSummary {
    id: Uuid,
    source_system: String,
    status: String,
    created_by: Option<String>,
    summary: serde_json::Value,
    created_at: chrono::DateTime<chrono::FixedOffset>,
    applied_at: Option<chrono::DateTime<chrono::FixedOffset>>,
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
        (status = 200, description = "Array of pairing plans without their entries", body = Object),
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

    Ok(Json(
        rows.into_iter()
            .map(
                |(id, source_system, status, created_by, summary, created_at, applied_at)| {
                    PairingPlanSummary {
                        id,
                        source_system,
                        status,
                        created_by,
                        summary,
                        created_at,
                        applied_at,
                    }
                },
            )
            .collect(),
    ))
}

/// Mark a draft superseded, which is what Start over does to the draft it replaces: the decisions
/// stay readable and `apply` refuses it as it refuses an applied plan. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/sync/pairing-plans/{id}/supersede",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    responses(
        (status = 200, description = "Plan superseded", body = Object),
        (status = 404, description = "Plan not found"),
        (status = 409, description = "Plan not in draft status"),
    ),
    tag = "sync"
)]
pub async fn supersede_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    let status = plan_status(&state.db, id).await?;
    if status != "draft" {
        return Err(AppError::Conflict(format!(
            "Plan is '{status}', can only supersede 'draft' plans"
        )));
    }
    state
        .db
        .execute_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE pairing_plans SET status = 'superseded' WHERE id = $1 AND status = 'draft'",
            [id.into()],
        ))
        .await?;
    Ok(Json(
        serde_json::json!({ "id": id, "status": "superseded" }),
    ))
}

/// Get a single pairing plan with its full pairing list. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/pairing-plans/{id}",
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
    /// The version the client read. The write is refused if the plan has moved on since.
    expected_version: i32,
    /// A plan-wide decision, applied before the per-entry updates so an operator can pair what
    /// matched, or skip what did not, in one act rather than one line per entry.
    #[serde(default)]
    bulk: Option<BulkAction>,
    #[serde(default)]
    updates: Vec<PlanEntryUpdate>,
    /// Standard curves to assign to instruments this plan will create. The move happens when the
    /// plan is applied, in the transaction that mints the instrument.
    #[serde(default)]
    curves: Vec<PlanCurveUpdate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BulkAction {
    #[serde(default)]
    r#where: crate::routes::private::sync::service::BulkWhere,
    /// `pair` or `skip`.
    action: String,
}

#[derive(Deserialize)]
struct PlanCurveUpdate {
    curve_id: Uuid,
    /// The `source_key` of an instrument the plan proposes creating. Null clears the assignment,
    /// leaving the curve on the instrument it has.
    #[serde(default)]
    instrument_source_key: Option<String>,
}

/// Fold the review's curve assignments into the plan's list. An assignment must name a curve that
/// exists and that no reading names yet (the curve's own update route refuses a used curve the
/// same way), and an instrument some paired entry proposes creating; anything else is a 400 now
/// rather than a failed apply later.
async fn apply_curve_updates(
    db: &sea_orm::DatabaseConnection,
    entries: &[crate::routes::private::sync::service::PlanEntry],
    intents: &mut Vec<crate::routes::private::sync::service::PlanCurveIntent>,
    updates: &[PlanCurveUpdate],
) -> AppResult<()> {
    for update in updates {
        intents.retain(|i| i.curve_id != update.curve_id);
        let Some(source_key) = update
            .instrument_source_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        else {
            continue;
        };
        let proposed = entries.iter().any(|e| {
            e.action == "pair"
                && e.instrument
                    .as_ref()
                    .is_some_and(|i| i.create && i.source_key == source_key)
        });
        if !proposed {
            return Err(AppError::BadRequest(format!(
                "this plan does not create an instrument with source key '{source_key}'; a \
                 curve can only be assigned here to an instrument the plan will create, an \
                 existing instrument takes it through the curve itself"
            )));
        }
        if crate::routes::private::sensors::standard_curves::Entity::find_by_id(update.curve_id)
            .one(db)
            .await?
            .is_none()
        {
            return Err(AppError::BadRequest(format!(
                "standard curve {} does not exist",
                update.curve_id
            )));
        }
        if crate::routes::private::sensors::standard_curves::views::curve_is_used(
            db,
            update.curve_id,
        )
        .await?
        {
            return Err(AppError::BadRequest(format!(
                "standard curve {} has already been applied to readings, so its instrument is \
                 fixed. Create a new curve on the new instrument and re-enter the affected \
                 measurements against it.",
                update.curve_id
            )));
        }
        intents.push(crate::routes::private::sync::service::PlanCurveIntent {
            curve_id: update.curve_id,
            instrument_source_key: source_key.to_string(),
        });
    }
    Ok(())
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
    /// Human display label for the parameter. Takes effect only when apply creates the
    /// parameter (`create: true`); a matched existing parameter keeps its own name.
    #[serde(default)]
    parameter_label: Option<String>,
    /// Point this entry's curve references at an existing lab instrument instead of the resolved
    /// one. Applies to every entry sharing the same curve column, since one column is one
    /// instrument across the source.
    #[serde(default)]
    instrument_id: Option<Uuid>,
    /// Rename an instrument the plan will create. Ignored once it resolves to an existing one.
    #[serde(default)]
    instrument_name: Option<String>,
    /// Agree to creating the proposed instrument. Apply refuses while any remain unconfirmed.
    #[serde(default)]
    instrument_confirmed: Option<bool>,
    /// Detach the instrument from every entry this one groups with. Attaching is `instrument_id`;
    /// this is its inverse, since an absent `instrument_id` means "unchanged", not "none".
    #[serde(default)]
    instrument_clear: Option<bool>,
    /// Declare which divisor this slot publishes its replicate standard deviation with,
    /// `sample` or `population`. Applied to the `site_parameters` row when the plan is applied.
    /// Never inferred: absent leaves the slot undeclared and the audit gate asks later.
    #[serde(default)]
    sd_estimator: Option<String>,
    /// Record that a person looked at this entry and agreed with it, or take that back. Separate
    /// from `action`, so changing what an entry does is not the same as deciding it.
    #[serde(default)]
    acknowledged: Option<bool>,
}

/// What an instrument decision covers. A curve column is one instrument across the whole source,
/// so settling it on any one entry settles every entry sharing the column; where no column names a
/// curve, the source parameter plays that role, so choosing the fluorometer for `chla_acid` covers
/// all 31 stations rather than one.
fn instrument_scope(entry: &crate::routes::private::sync::service::PlanEntry) -> String {
    match entry
        .instrument
        .as_ref()
        .and_then(|i| i.curve_column.as_deref())
    {
        Some(column) => format!("column:{column}"),
        None => format!(
            "parameter:{}",
            entry
                .parameter
                .group_key
                .as_deref()
                .unwrap_or(&entry.parameter.name)
        ),
    }
}

/// Apply the instrument half of a plan edit.
///
/// Kept apart from the per-entry loop because an instrument decision is per curve column, not per
/// stream: one column resolves to one instrument across the whole source, so confirming or
/// repointing it on any one entry settles every entry that shares it. Doing it per entry would
/// leave 30 of 31 DOC streams still asking.
async fn apply_instrument_updates(
    state: &AppState,
    source_system: &str,
    entries: &mut [crate::routes::private::sync::service::PlanEntry],
    updates: &[PlanEntryUpdate],
) -> AppResult<()> {
    for update in updates {
        if update.instrument_id.is_none()
            && update.instrument_name.is_none()
            && update.instrument_confirmed.is_none()
            && update.instrument_clear != Some(true)
        {
            continue;
        }
        let Some(target) = entries.iter().find(|e| e.stream_id == update.stream_id) else {
            continue;
        };
        let scope = instrument_scope(target);

        // A feed the source reports as a device has its instrument already: one minted for the
        // slot it serves when the stream is paired, with that slot's deployment opened. Minting a
        // lab instrument for it instead would take both.
        if update.instrument_name.is_some() && update.instrument_id.is_none() && target.is_device {
            let named = match &target.device_serial {
                Some(serial) => format!(" (the source names device serial {serial})"),
                None => String::new(),
            };
            return Err(AppError::BadRequest(format!(
                "stream {} is reported as a device{named}, so its instrument is minted for the \
                 slot it serves when the stream is paired. Attach an existing instrument to \
                 override that, or leave it unset.",
                update.stream_id
            )));
        }

        // A repoint has to name an instrument that exists; otherwise the plan would carry an id
        // the apply cannot resolve.
        let repointed = match update.instrument_id {
            Some(id) => Some(
                sensors::Entity::find_by_id(id)
                    .one(&state.db)
                    .await?
                    .ok_or_else(|| {
                        AppError::BadRequest(format!("Instrument {id} does not exist"))
                    })?,
            ),
            None => None,
        };
        // The chosen instrument's curves travel with the entry, so the review shows what it
        // corrects with rather than only its name.
        let repointed_curves = match &repointed {
            Some(sensor) => crate::routes::private::sensors::standard_curves::Entity::find()
                .filter(
                    crate::routes::private::sensors::standard_curves::Column::SensorId
                        .eq(sensor.id),
                )
                .all(&state.db)
                .await?
                .into_iter()
                .map(|c| crate::routes::private::sync::service::PlanCurveRef {
                    id: c.id,
                    name: c.name,
                    slope: c.slope,
                    intercept: c.intercept,
                })
                .collect(),
            None => Vec::new(),
        };

        for entry in entries.iter_mut().filter(|e| instrument_scope(e) == scope) {
            if update.instrument_clear == Some(true) {
                entry.instrument = None;
                continue;
            }
            // A stream whose source names no curve per reading has no instrument until someone
            // says which one corrected it upstream. Attaching an existing one records that, and
            // naming a new one proposes it; neither stamps, because the value already carries the
            // correction. The identity is the parameter, so every station moves together.
            if entry.instrument.is_none() {
                let proposed = update
                    .instrument_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty());
                if repointed.is_some() || proposed.is_some() {
                    let parameter = entry.parameter.name.clone();
                    let name = proposed
                        .map(str::to_string)
                        .unwrap_or_else(|| parameter.clone());
                    entry.instrument =
                        Some(crate::routes::private::sync::service::PlanInstrumentRef {
                            curve_column: None,
                            id: None,
                            name: name.clone(),
                            source_key: format!("{source_system}:{parameter}"),
                            resolved_by: if repointed.is_some() {
                                "manual".to_string()
                            } else {
                                "placeholder".to_string()
                            },
                            create: repointed.is_none(),
                            confirmed: repointed.is_some(),
                            stamps_readings: false,
                            curves: Vec::new(),
                            proposed_name: Some(name),
                        });
                }
            }
            let entry_parameter = entry.parameter.name.clone();
            let Some(instrument) = entry.instrument.as_mut() else {
                continue;
            };
            if let Some(sensor) = &repointed {
                instrument.id = Some(sensor.id);
                instrument.name = sensor
                    .name
                    .clone()
                    .or_else(|| sensor.serial_number.clone())
                    .unwrap_or_else(|| sensor.id.to_string());
                instrument.source_key = sensor.source_key.clone().unwrap_or_default();
                instrument.resolved_by = "manual".to_string();
                instrument.create = false;
                instrument.confirmed = true;
                instrument.curves = repointed_curves.clone();
            }
            // Naming an instrument proposes one; picking from the inventory attaches one. So a
            // name arriving at an entry that holds an existing instrument returns it to a
            // proposal, rather than doing nothing (which is what an operator undoing a mis-click
            // used to get) or renaming the inventory row (which this route never does).
            if let Some(name) = &update.instrument_name
                && repointed.is_none()
                && !name.trim().is_empty()
            {
                let name = name.trim().to_string();
                if instrument.create {
                    instrument.name = name.clone();
                    instrument.proposed_name = Some(name);
                } else {
                    let source_key = match &instrument.curve_column {
                        Some(column) => format!("{source_system}:{column}"),
                        None => format!("{source_system}:{entry_parameter}"),
                    };
                    instrument.id = None;
                    instrument.name = name.clone();
                    instrument.proposed_name = Some(name);
                    instrument.source_key = source_key;
                    instrument.resolved_by = "placeholder".to_string();
                    instrument.create = true;
                    instrument.confirmed = false;
                    instrument.curves = Vec::new();
                }
            }

            if let Some(confirmed) = update.instrument_confirmed {
                instrument.confirmed = confirmed;
            }
        }
    }
    Ok(())
}

/// Edit a draft pairing plan (only `draft` status allows updates). Requires `write_metadata`.
#[utoipa::path(
    patch,
    path = "/api/sync/pairing-plans/{id}",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    request_body(content = Object),
    responses(
        (status = 200, description = "Updated plan", body = Object),
        (status = 404, description = "Plan not found"),
        (status = 409, description = "Plan not in draft status, or edited since the client read it", body = Object),
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
        return Err(AppError::Conflict(format!(
            "Plan is '{}', can only edit 'draft' plans",
            plan.status
        )));
    }
    if plan.version != req.expected_version {
        return Err(stale_plan(plan.version));
    }

    let mut entries: Vec<crate::routes::private::sync::service::PlanEntry> =
        serde_json::from_value(plan.entries.clone())
            .map_err(|e| AppError::Internal(format!("Failed to parse entries: {e}")))?;

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

    apply_instrument_updates(&state, &plan.source_system, &mut entries, &req.updates).await?;

    let mut intents = crate::routes::private::sync::service::plan_curve_intents(&plan)?;
    apply_curve_updates(&state.db, &entries, &mut intents, &req.curves).await?;

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
             version = version + 1 WHERE id = $4 AND version = $5",
            [
                serde_json::to_value(&entries).unwrap_or_default().into(),
                serde_json::to_value(&intents).unwrap_or_default().into(),
                summary.into(),
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
    Ok(Json(updated))
}

/// The refusal a writer gets when the draft has moved on: the version it should reload is in the
/// detail, so the client re-reads and re-applies rather than guessing.
fn stale_plan(current_version: i32) -> AppError {
    AppError::ConflictDetail {
        message: "The plan changed since you read it; reload it and reapply your edits".to_string(),
        detail: serde_json::json!({ "current_version": current_version }),
    }
}

/// Fetch a pairing plan's version, or 404 if unknown.
async fn plan_version(db: &sea_orm::DatabaseConnection, id: Uuid) -> AppResult<i32> {
    let row = db
        .query_one_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT version FROM pairing_plans WHERE id = $1",
            [id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    Ok(row.try_get::<i32>("", "version")?)
}

#[derive(Deserialize)]
pub struct ApplyPairingPlanRequest {
    /// The version the client read. Applying a draft someone else has edited since is refused.
    expected_version: i32,
}

/// Apply a pairing plan: execute all its pairings and backfills atomically. Marks the
/// plan as `applied`. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/sync/pairing-plans/{id}/apply",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    request_body(content = Object),
    responses(
        (status = 200, description = "Plan applied with execution counts", body = Object),
        (status = 404, description = "Plan not found"),
        (status = 409, description = "Plan already applied or reverted, or edited since the client read it", body = Object),
    ),
    tag = "sync"
)]
pub async fn apply_pairing_plan(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<ApplyPairingPlanRequest>,
) -> AppResult<Json<serde_json::Value>> {
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
    let entries: Vec<crate::routes::private::sync::service::PlanEntry> =
        serde_json::from_value(plan.entries)
            .map_err(|e| AppError::Internal(format!("Failed to parse entries: {e}")))?;
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
    Ok(Json(
        serde_json::json!({ "job_id": job_id, "status": "queued" }),
    ))
}

/// Fetch a pairing plan's status, or 404 if unknown.
async fn plan_status(db: &sea_orm::DatabaseConnection, id: Uuid) -> AppResult<String> {
    let row = db
        .query_one_raw(sea_orm::Statement::from_sql_and_values(
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
    path = "/api/sync/pairing-plans/{id}/revert",
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
    Ok(Json(
        serde_json::json!({ "job_id": job_id, "status": "queued" }),
    ))
}

/// Aggregate summary of unpaired streams grouped by source system. Used by the dashboard
/// to surface streams needing attention. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/unpaired-summary",
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
        .query_all_raw(Statement::from_string(
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
    path = "/api/sync/pairing-plans/{id}/site-metadata",
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
            "channel_id": get("channel_id"),
            "sample_interval_sec": get("sample_interval_sec").and_then(|s| s.parse::<i64>().ok()),
        })
    }).collect();

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
    let mut devices_by_site: std::collections::HashMap<String, Vec<serde_json::Value>> =
        std::collections::HashMap::new();
    for row in &device_rows {
        let site: String = row.try_get("", "site_name").unwrap_or_default();
        let serial: String = row.try_get("", "serial").unwrap_or_default();
        if serial.is_empty() {
            continue;
        }
        devices_by_site
            .entry(site)
            .or_default()
            .push(serde_json::json!({
                "serial": serial,
                "model": row.try_get::<Option<String>>("", "model").ok().flatten(),
                "streams": row.try_get::<i64>("", "streams").unwrap_or_default(),
            }));
    }

    let result: Vec<serde_json::Value> = result
        .into_iter()
        .map(|mut site| {
            let name = site
                .get("site_name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            site["devices"] =
                serde_json::Value::Array(devices_by_site.get(&name).cloned().unwrap_or_default());
            site
        })
        .collect();

    Ok(Json(result))
}

/// One instrument decision in a pairing plan: the instrument, what it covers, and the curves it
/// owns. Only instruments the plan actually binds are listed; the rest of the inventory is
/// reachable through the picker, so this stays a list of decisions rather than a catalog.
#[derive(Debug, Serialize)]
pub struct PlanInstrumentGroup {
    /// The decision's scope: `column:<curve column>` or `parameter:<source parameter>`, matching
    /// what an update to any member stream settles. Absent for an unbound instrument.
    pub scope: Option<String>,
    pub instrument_id: Option<Uuid>,
    pub name: String,
    pub source_key: String,
    /// `stream` | `curve_label` | `manual` | `placeholder`.
    pub resolved_by: String,
    pub create: bool,
    pub confirmed: bool,
    /// Whether readings under this decision will store a `standard_curve_id`.
    pub stamps_readings: bool,
    pub curve_column: Option<String>,
    pub stream_count: usize,
    pub parameters: Vec<String>,
    pub site_count: usize,
    /// A stream to address an update to; every entry in the same scope moves with it.
    pub anchor_stream_id: Option<Uuid>,
    pub curves: Vec<crate::routes::private::sync::service::PlanCurveRef>,
    /// What this decision proposed creating, kept through an attach so the picker can offer it
    /// back. Absent for an instrument that was never a proposal.
    pub proposed_name: Option<String>,
}

/// The source parameters this plan pairs that no instrument covers, so an operator can attach one
/// where the portal corrected a value upstream without naming a curve per reading.
#[derive(Debug, Serialize)]
pub struct PlanUnassignedParameter {
    pub scope: String,
    pub parameter: String,
    pub stream_count: usize,
    pub site_count: usize,
    pub anchor_stream_id: Uuid,
    /// The name an instrument for this parameter would get, proposed the way a site's or a
    /// parameter's name is. Accepting it is what creates the instrument; nothing is minted from a
    /// suggestion alone.
    pub suggested_name: String,
}

/// One standard curve the source has replicated, and the instrument it is currently fitted on.
/// Re-homing is a curve-level decision, so the curves are listed in their own right rather than
/// only inside the instrument that happens to own them.
#[derive(Debug, Serialize)]
pub struct PlanCurveAssignment {
    pub id: Uuid,
    pub name: Option<String>,
    pub slope: f64,
    pub intercept: f64,
    pub r_squared: Option<f64>,
    pub source_key: Option<String>,
    pub sensor_id: Uuid,
    pub instrument_name: String,
    /// Readings this curve has already corrected. A curve with history is one whose instrument a
    /// re-home changes the meaning of, so the number is shown beside the choice.
    pub reading_count: i64,
    /// The instrument this plan will move the curve onto when applied, by `source_key`, and the
    /// name the plan proposes for it. Absent when no assignment is pending.
    pub pending_source_key: Option<String>,
    pub pending_instrument_name: Option<String>,
}

/// One device-shaped feed a plan carries, and the slot it serves.
///
/// A device is not a decision the plan takes: the feed's own `(source_system, source_key)` is the
/// identity, and pairing mints the instrument for its slot and opens that slot's deployment. One
/// instrument serves one (site, parameter), so a multi-channel logger is one group per channel
/// rather than one group carrying them all; the serial it reports is displayed, never matched on.
/// It is listed so an operator can see which instrument each feed will land on, and whether it is
/// already in the inventory.
#[derive(Debug, Serialize)]
pub struct PlanDeviceGroup {
    pub site: String,
    pub serial: String,
    pub model: Option<String>,
    /// The inventory row this serial already resolves to, when it has one.
    pub instrument_id: Option<Uuid>,
    pub instrument_name: Option<String>,
    pub parameters: Vec<String>,
    pub stream_count: usize,
    pub anchor_stream_id: Uuid,
}

#[derive(Debug, Serialize)]
pub struct PlanInstrumentsResponse {
    pub groups: Vec<PlanInstrumentGroup>,
    pub unassigned: Vec<PlanUnassignedParameter>,
    pub devices: Vec<PlanDeviceGroup>,
    pub curves: Vec<PlanCurveAssignment>,
}

/// The instrument picture of a pairing plan: every instrument the plan binds, and the parameters
/// still without one. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/sync/pairing-plans/{id}/instruments",
    params(("id" = Uuid, Path, description = "Pairing plan UUID")),
    responses(
        (status = 200, description = "The plan's instruments and unassigned parameters", body = Object),
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
    let entries: Vec<crate::routes::private::sync::service::PlanEntry> =
        serde_json::from_value(plan.entries.clone()).unwrap_or_default();

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
        let scope = instrument_scope(entry);
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
                            suggested_name: format!(
                                "{} {}",
                                entry.parameter.name, plan.source_system
                            ),
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
