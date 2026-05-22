use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, EntityTrait, QueryFilter,
    QueryOrder, Set, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::routes::private::{data_streams, parameters, pairing_plans, projects, site_parameters, sites};
use crate::error::{AppError, AppResult};
use crate::routes::private::sensors::operations::create_sensor_for_stream;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamHierarchy {
    pub project: String,
    pub site: String,
    pub parameter: String,
    pub units: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub altitude_m: Option<f64>,
}

/// Extract the project/site/parameter hierarchy from a stream's metadata.
///
/// Priority:
/// 1. metadata.hierarchy (set by all portal backends)
/// 2. source_path segment parsing (fallback)
/// 3. source_name splitting on " - " (last resort)
pub fn extract_hierarchy(stream: &data_streams::Model) -> StreamHierarchy {
    let meta = &stream.metadata;

    // Try metadata.hierarchy first
    if let Some(h) = meta.get("hierarchy") {
        let project = h.get("project").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let site = h.get("site").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let parameter = h.get("parameter").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let units = meta.get("units").and_then(|v| v.as_str()).unwrap_or("").to_string();

        let coords = meta.get("coordinates");
        let lat = coords.and_then(|c| c.get("latitude")).and_then(|v| v.as_f64());
        let lon = coords.and_then(|c| c.get("longitude")).and_then(|v| v.as_f64());
        let alt = coords.and_then(|c| c.get("altitude_m")).and_then(|v| v.as_f64());

        if !project.is_empty() || !site.is_empty() || !parameter.is_empty() {
            return StreamHierarchy { project, site, parameter, units, latitude: lat, longitude: lon, altitude_m: alt };
        }
    }

    // Fallback: source_path segment parsing
    if let Some(ref path) = stream.source_path {
        let segs: Vec<&str> = path.split('/').collect();
        let project = segs.get(1).unwrap_or(&"").to_string();
        let site = segs.get(2).unwrap_or(&"").to_string();
        let parameter = segs.get(3).unwrap_or(&"").to_string();
        let units = meta.get("units").and_then(|v| v.as_str()).unwrap_or("").to_string();

        return StreamHierarchy {
            project, site, parameter, units,
            latitude: None, longitude: None, altitude_m: None,
        };
    }

    // Last resort: source_name
    let parameter = stream.source_name.as_deref()
        .and_then(|n| n.split(" - ").nth(1))
        .unwrap_or("")
        .to_string();
    let units = meta.get("units").and_then(|v| v.as_str()).unwrap_or("").to_string();

    StreamHierarchy {
        project: stream.source_system.to_uppercase(),
        site: String::new(),
        parameter,
        units,
        latitude: None, longitude: None, altitude_m: None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEntry {
    pub stream_id: Uuid,
    pub source_key: String,
    pub source_name: Option<String>,
    pub action: String, // "pair" | "skip"
    pub project: PlanEntityRef,
    pub site: PlanSiteRef,
    pub parameter: PlanParamRef,
    pub confidence: String, // "exact" | "fuzzy" | "none"
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEntityRef {
    pub id: Option<Uuid>,
    pub name: String,
    pub create: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSiteRef {
    pub id: Option<Uuid>,
    pub name: String,
    pub create: bool,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub altitude_m: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanParamRef {
    pub id: Option<Uuid>,
    pub name: String,
    pub create: bool,
    pub units: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSummary {
    pub total_streams: usize,
    pub will_pair: usize,
    pub will_skip: usize,
    pub projects_to_create: usize,
    pub sites_to_create: usize,
    pub parameters_to_create: usize,
    pub unique_projects: usize,
    pub unique_sites: usize,
    pub unique_parameters: usize,
}

/// Create a pairing plan for all unpaired streams of a given source system.
pub async fn create_plan(
    db: &impl ConnectionTrait,
    source_system: &str,
) -> AppResult<pairing_plans::Model> {
    let streams = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq(source_system))
        .filter(data_streams::Column::SiteParameterId.is_null())
        .order_by_asc(data_streams::Column::SourceKey)
        .all(db)
        .await?;

    if streams.is_empty() {
        return Err(AppError::BadRequest(format!(
            "No unpaired streams found for source_system '{source_system}'"
        )));
    }

    // Load existing entities for matching
    let existing_projects: Vec<(Uuid, String)> = projects::Entity::find()
        .all(db).await?
        .into_iter().map(|p| (p.id, p.name)).collect();

    let existing_sites: Vec<(Uuid, String)> = sites::Entity::find()
        .all(db).await?
        .into_iter().map(|s| (s.id, s.name)).collect();

    let existing_params: Vec<(Uuid, String, String)> = parameters::Entity::find()
        .all(db).await?
        .into_iter().map(|p| (p.id, p.name.clone(), p.default_units)).collect();

    // Build entries
    let mut entries: Vec<PlanEntry> = Vec::with_capacity(streams.len());

    for stream in &streams {
        let h = extract_hierarchy(stream);

        // Match project
        let (proj_id, proj_create) = match_entity(&h.project, &existing_projects);
        let project_confidence = if proj_id.is_some() { "exact" } else { "none" };

        // Match site
        let (site_id, site_create) = match_entity(&h.site, &existing_sites);
        let site_confidence = if site_id.is_some() { "exact" } else { "none" };

        // Match parameter
        let (param_id, param_create) = match_entity_display(&h.parameter, &existing_params);
        let param_confidence = if param_id.is_some() { "exact" } else { "none" };

        // Check for parameter unit collision
        let mut warnings = Vec::new();
        if let Some(pid) = param_id
            && let Some((_, _, existing_units)) = existing_params.iter().find(|(id, _, _)| *id == pid)
            && !existing_units.is_empty() && !h.units.is_empty()
            && existing_units.to_lowercase() != h.units.to_lowercase()
        {
            warnings.push(format!(
                "Parameter '{}' exists with units '{}' but this source uses '{}'",
                h.parameter, existing_units, h.units
            ));
        }

        // Overall confidence: lowest of the three
        let confidence = if project_confidence == "exact" && site_confidence == "exact" && param_confidence == "exact" {
            "exact"
        } else {
            "none"
        }.to_string();

        let action = if h.site.is_empty() || h.parameter.is_empty() {
            "skip".to_string()
        } else {
            "pair".to_string()
        };

        entries.push(PlanEntry {
            stream_id: stream.id,
            source_key: stream.source_key.clone(),
            source_name: stream.source_name.clone(),
            action,
            project: PlanEntityRef {
                id: proj_id,
                name: h.project,
                create: proj_create,
            },
            site: PlanSiteRef {
                id: site_id,
                name: h.site,
                create: site_create,
                latitude: h.latitude,
                longitude: h.longitude,
                altitude_m: h.altitude_m,
            },
            parameter: PlanParamRef {
                id: param_id,
                name: h.parameter,
                create: param_create,
                units: h.units,
            },
            confidence,
            warnings,
        });
    }

    let summary = compute_summary(&entries);

    let plan = pairing_plans::ActiveModel {
        id: Set(Uuid::new_v4()),
        source_system: Set(source_system.to_string()),
        status: Set("draft".to_string()),
        created_by: Set(None),
        summary: Set(serde_json::to_value(&summary).unwrap_or_default()),
        entries: Set(serde_json::to_value(&entries).unwrap_or_default()),
        created_at: Set(Utc::now().into()),
        applied_at: Set(None),
        apply_result: Set(None),
    };

    let inserted = plan.insert(db).await?;
    Ok(inserted)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApplyResult {
    pub projects_created: u32,
    pub sites_created: u32,
    pub parameters_created: u32,
    pub site_parameters_created: u32,
    pub streams_paired: u32,
    pub readings_backfilled: u64,
}

struct EntityCaches {
    projects: HashMap<String, Uuid>,
    sites: HashMap<String, Uuid>,
    params: HashMap<String, Uuid>,
    site_params: HashMap<(Uuid, Uuid), Uuid>,
    param_names: HashMap<Uuid, String>,
}

struct ApplyCounters {
    projects_created: u32,
    sites_created: u32,
    params_created: u32,
    sp_created: u32,
    streams_paired: u32,
}

/// Apply a pairing plan: create entities, pair streams, backfill readings.
pub async fn apply_plan(
    db: &sea_orm::DatabaseConnection,
    plan_id: Uuid,
) -> AppResult<ApplyResult> {
    let plan = pairing_plans::Entity::find_by_id(plan_id)
        .one(db).await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    if plan.status != "draft" {
        return Err(AppError::BadRequest(format!(
            "Plan is '{}', can only apply 'draft' plans", plan.status
        )));
    }

    let entries: Vec<PlanEntry> = serde_json::from_value(plan.entries.clone())
        .map_err(|e| AppError::Internal(format!("Failed to parse plan entries: {e}")))?;

    let txn = db.begin().await?;

    txn.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0".to_owned(),
    )).await?;

    let param_names: HashMap<Uuid, String> = parameters::Entity::find()
        .all(&txn).await?
        .into_iter().map(|p| (p.id, p.name)).collect();

    let mut caches = EntityCaches {
        projects: HashMap::new(),
        sites: HashMap::new(),
        params: HashMap::new(),
        site_params: HashMap::new(),
        param_names,
    };
    let mut counters = ApplyCounters {
        projects_created: 0, sites_created: 0, params_created: 0,
        sp_created: 0, streams_paired: 0,
    };

    for entry in entries.iter().filter(|e| e.action == "pair") {
        let (site_parameter_id, parameter_id) = resolve_plan_entry(
            &txn, entry, &plan.source_system, &mut caches, &mut counters,
        ).await?;
        pair_entry_stream(&txn, entry, plan_id, site_parameter_id, parameter_id).await?;
        counters.streams_paired += 1;
    }

    let readings_backfilled = backfill_plan_readings(&txn, plan_id).await?;
    finalize_plan(&txn, plan_id, &counters, readings_backfilled).await?;
    txn.commit().await?;

    let db_clone = db.clone();
    tokio::spawn(async move {
        crate::common::sync_state::refresh_continuous_aggregates_full(&db_clone).await;
    });

    let result = ApplyResult {
        projects_created: counters.projects_created,
        sites_created: counters.sites_created,
        parameters_created: counters.params_created,
        site_parameters_created: counters.sp_created,
        streams_paired: counters.streams_paired,
        readings_backfilled,
    };

    tracing::info!(
        plan_id = %plan_id,
        streams_paired = counters.streams_paired,
        sites_created = counters.sites_created,
        params_created = counters.params_created,
        readings_backfilled,
        "Pairing plan applied"
    );

    Ok(result)
}

/// Resolve or create all entities for one plan entry. Returns (site_parameter_id, parameter_id).
async fn resolve_plan_entry<C: ConnectionTrait>(
    txn: &C,
    entry: &PlanEntry,
    source_system: &str,
    caches: &mut EntityCaches,
    counters: &mut ApplyCounters,
) -> AppResult<(Uuid, Uuid)> {
    let project_id = resolve_or_create_project(
        txn, &entry.project, &mut caches.projects, &mut counters.projects_created, source_system,
    ).await?;
    let site_id = resolve_or_create_site(
        txn, &entry.site, &mut caches.sites, &mut counters.sites_created, project_id,
    ).await?;
    let parameter_id = resolve_or_create_param(
        txn, &entry.parameter, &mut caches.params, &mut counters.params_created,
    ).await?;
    let site_parameter_id = resolve_or_create_site_param(
        txn, site_id, parameter_id, &caches.param_names, &caches.params,
        &mut caches.site_params, &mut counters.sp_created,
    ).await?;
    Ok((site_parameter_id, parameter_id))
}

async fn resolve_or_create_site_param<C: ConnectionTrait>(
    txn: &C,
    site_id: Uuid,
    parameter_id: Uuid,
    param_names: &HashMap<Uuid, String>,
    param_cache: &HashMap<String, Uuid>,
    sp_cache: &mut HashMap<(Uuid, Uuid), Uuid>,
    sp_created: &mut u32,
) -> AppResult<Uuid> {
    let key = (site_id, parameter_id);
    if let Some(&id) = sp_cache.get(&key) {
        return Ok(id);
    }

    let existing = site_parameters::Entity::find()
        .filter(Condition::all()
            .add(site_parameters::Column::SiteId.eq(site_id))
            .add(site_parameters::Column::ParameterId.eq(parameter_id)))
        .one(txn).await?;

    let id = if let Some(existing) = existing {
        existing.id
    } else {
        let id = Uuid::new_v4();
        let param_name_val = param_names.get(&parameter_id)
            .or_else(|| param_cache.iter().find(|&(_, &v)| v == parameter_id).map(|(k, _)| k))
            .cloned()
            .unwrap_or_default();
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
        }.insert(txn).await?;
        *sp_created += 1;
        id
    };
    sp_cache.insert(key, id);
    Ok(id)
}

async fn pair_entry_stream<C: ConnectionTrait>(
    txn: &C,
    entry: &PlanEntry,
    plan_id: Uuid,
    site_parameter_id: Uuid,
    parameter_id: Uuid,
) -> AppResult<()> {
    let stream = data_streams::Entity::find_by_id(entry.stream_id)
        .one(txn).await?
        .ok_or_else(|| AppError::NotFound(format!("Stream {} not found", entry.stream_id)))?;

    if stream.sensor_id.is_none() {
        let site_id = site_parameters::Entity::find_by_id(site_parameter_id)
            .one(txn).await?
            .map(|sp| sp.site_id)
            .unwrap_or_default();
        if let Err(e) = create_sensor_for_stream(txn, &stream, parameter_id, site_id).await {
            tracing::warn!(
                error = %e,
                stream_id = %stream.id,
                parameter_id = %parameter_id,
                site_id = %site_id,
                "Failed to auto-create sensor for stream during pairing; stream will still be paired",
            );
        }
    }

    let now = Utc::now();
    let mut active: data_streams::ActiveModel = stream.into();
    active.site_parameter_id = Set(Some(site_parameter_id));
    active.pairing_plan_id = Set(Some(plan_id));
    active.paired_at = Set(Some(now.into()));
    active.updated_at = Set(now.into());
    active.update(txn).await?;
    Ok(())
}

async fn backfill_plan_readings<C: ConnectionTrait>(txn: &C, plan_id: Uuid) -> AppResult<u64> {
    let backfill_result = txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE readings r
          SET site_id = sp.site_id, parameter_id = sp.parameter_id,
              calibrated_value = COALESCE(r.calibrated_value, r.raw_value)
          FROM data_streams ds
          JOIN site_parameters sp ON ds.site_parameter_id = sp.id
          WHERE r.stream_id = ds.id AND r.site_id IS NULL
            AND ds.pairing_plan_id = $1",
        [plan_id.into()],
    )).await?;

    if let Err(e) = txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE status_events se
          SET site_id = sp.site_id, parameter_id = sp.parameter_id
          FROM data_streams ds
          JOIN site_parameters sp ON ds.site_parameter_id = sp.id
          WHERE se.stream_id = ds.id AND se.site_id IS NULL
            AND ds.pairing_plan_id = $1",
        [plan_id.into()],
    )).await {
        tracing::warn!(
            error = %e,
            plan_id = %plan_id,
            "Failed to backfill status_events site_id/parameter_id during plan apply; readings were still updated",
        );
    }

    Ok(backfill_result.rows_affected())
}

async fn finalize_plan<C: ConnectionTrait>(
    txn: &C,
    plan_id: Uuid,
    counters: &ApplyCounters,
    readings_backfilled: u64,
) -> AppResult<()> {
    let result = ApplyResult {
        projects_created: counters.projects_created,
        sites_created: counters.sites_created,
        parameters_created: counters.params_created,
        site_parameters_created: counters.sp_created,
        streams_paired: counters.streams_paired,
        readings_backfilled,
    };

    let mut plan_active: pairing_plans::ActiveModel = pairing_plans::Entity::find_by_id(plan_id)
        .one(txn).await?
        .ok_or_else(|| AppError::Internal("Plan disappeared during apply".to_string()))?
        .into();
    plan_active.status = Set("applied".to_string());
    plan_active.applied_at = Set(Some(Utc::now().into()));
    plan_active.apply_result = Set(Some(serde_json::to_value(&result).unwrap_or_default()));
    plan_active.update(txn).await?;
    Ok(())
}

/// Revert a pairing plan: bulk unpair all streams that were paired by this plan.
pub async fn revert_plan(
    db: &sea_orm::DatabaseConnection,
    plan_id: Uuid,
) -> AppResult<u32> {
    let plan = pairing_plans::Entity::find_by_id(plan_id)
        .one(db).await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    if plan.status != "applied" {
        return Err(AppError::BadRequest(format!(
            "Plan is '{}', can only revert 'applied' plans", plan.status
        )));
    }

    let txn = db.begin().await?;

    txn.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0".to_owned(),
    )).await?;

    // NULL out readings for streams from this plan
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE readings r SET site_id = NULL, parameter_id = NULL
          FROM data_streams ds
          WHERE r.stream_id = ds.id AND ds.pairing_plan_id = $1",
        [plan_id.into()],
    )).await?;

    // NULL out status_events — best-effort: readings are already cleared above
    if let Err(e) = txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE status_events se SET site_id = NULL, parameter_id = NULL
          FROM data_streams ds
          WHERE se.stream_id = ds.id AND ds.pairing_plan_id = $1",
        [plan_id.into()],
    )).await {
        tracing::warn!(
            error = %e,
            plan_id = %plan_id,
            "Failed to NULL status_events during plan revert; readings were already cleared",
        );
    }

    // Unpair the streams
    let result = txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE data_streams SET site_parameter_id = NULL, paired_at = NULL, pairing_plan_id = NULL
          WHERE pairing_plan_id = $1",
        [plan_id.into()],
    )).await?;
    let reverted = result.rows_affected() as u32;

    // Update plan status
    let mut plan_active: pairing_plans::ActiveModel = pairing_plans::Entity::find_by_id(plan_id)
        .one(&txn).await?
        .ok_or_else(|| AppError::Internal("Plan disappeared during revert".to_string()))?
        .into();
    plan_active.status = Set("reverted".to_string());
    plan_active.update(&txn).await?;

    txn.commit().await?;

    // Refresh aggregates synchronously so callers see consistent state
    crate::common::sync_state::refresh_continuous_aggregates_full(db).await;

    tracing::info!(plan_id = %plan_id, reverted, "Pairing plan reverted");
    Ok(reverted)
}

pub fn compute_summary_pub(entries: &[PlanEntry]) -> PlanSummary {
    compute_summary(entries)
}

fn compute_summary(entries: &[PlanEntry]) -> PlanSummary {
    let will_pair = entries.iter().filter(|e| e.action == "pair").count();
    let will_skip = entries.iter().filter(|e| e.action == "skip").count();

    let unique_projects: std::collections::HashSet<&str> = entries.iter()
        .filter(|e| e.action == "pair")
        .map(|e| e.project.name.as_str())
        .collect();
    let unique_sites: std::collections::HashSet<&str> = entries.iter()
        .filter(|e| e.action == "pair")
        .map(|e| e.site.name.as_str())
        .collect();
    let unique_params: std::collections::HashSet<&str> = entries.iter()
        .filter(|e| e.action == "pair")
        .map(|e| e.parameter.name.as_str())
        .collect();

    let projects_to_create = entries.iter()
        .filter(|e| e.action == "pair" && e.project.create)
        .map(|e| &e.project.name)
        .collect::<std::collections::HashSet<_>>().len();
    let sites_to_create = entries.iter()
        .filter(|e| e.action == "pair" && e.site.create)
        .map(|e| &e.site.name)
        .collect::<std::collections::HashSet<_>>().len();
    let params_to_create = entries.iter()
        .filter(|e| e.action == "pair" && e.parameter.create)
        .map(|e| &e.parameter.name)
        .collect::<std::collections::HashSet<_>>().len();

    PlanSummary {
        total_streams: entries.len(),
        will_pair,
        will_skip,
        projects_to_create,
        sites_to_create,
        parameters_to_create: params_to_create,
        unique_projects: unique_projects.len(),
        unique_sites: unique_sites.len(),
        unique_parameters: unique_params.len(),
    }
}

fn match_entity(name: &str, existing: &[(Uuid, String)]) -> (Option<Uuid>, bool) {
    if name.is_empty() {
        return (None, false);
    }
    let lower = name.to_lowercase();
    if let Some((id, _)) = existing.iter().find(|(_, n)| n.to_lowercase() == lower) {
        (Some(*id), false)
    } else {
        (None, true)
    }
}

fn match_entity_display(name: &str, existing: &[(Uuid, String, String)]) -> (Option<Uuid>, bool) {
    if name.is_empty() {
        return (None, false);
    }
    let lower = name.to_lowercase();
    if let Some((id, _, _)) = existing.iter().find(|(_, n, _)| n.to_lowercase() == lower) {
        (Some(*id), false)
    } else {
        (None, true)
    }
}

use sea_orm::sea_query::Expr;

async fn resolve_or_create_project<C: ConnectionTrait>(
    txn: &C,
    entity_ref: &PlanEntityRef,
    cache: &mut HashMap<String, Uuid>,
    created_count: &mut u32,
    source_system: &str,
) -> AppResult<Uuid> {
    if let Some(id) = entity_ref.id {
        return Ok(id);
    }
    let key = entity_ref.name.to_lowercase();
    if let Some(&id) = cache.get(&key) {
        return Ok(id);
    }
    let existing = projects::Entity::find()
        .filter(Expr::cust_with_values("LOWER(name) = $1", [key.clone()]))
        .one(txn).await?;
    if let Some(existing) = existing {
        cache.insert(key, existing.id);
        return Ok(existing.id);
    }
    let id = Uuid::new_v4();
    projects::ActiveModel {
        id: Set(id),
        name: Set(entity_ref.name.clone()),
        description: Set(None),
        data_source: Set(Some(source_system.to_string())),
        is_public: Set(false),
        public_slug: Set(None),
        public_api_title: Set(None),
        public_api_description: Set(None),
        public_api_version: Set(None),
        public_contact_email: Set(None),
        created_at: Set(Some(Utc::now())),
        discovered_at: Set(Some(Utc::now())),
    }.insert(txn).await?;
    *created_count += 1;
    cache.insert(key, id);
    Ok(id)
}

async fn resolve_or_create_site(
    txn: &impl ConnectionTrait,
    site_ref: &PlanSiteRef,
    cache: &mut HashMap<String, Uuid>,
    created_count: &mut u32,
    project_id: Uuid,
) -> AppResult<Uuid> {
    if let Some(id) = site_ref.id {
        return Ok(id);
    }
    let key = site_ref.name.to_lowercase();
    if let Some(&id) = cache.get(&key) {
        return Ok(id);
    }
    let existing = sites::Entity::find()
        .filter(Expr::cust_with_values("LOWER(name) = $1", [key.clone()]))
        .one(txn).await?;
    if let Some(existing) = existing {
        if existing.latitude.is_none() && site_ref.latitude.is_some() {
            let mut update: sites::ActiveModel = existing.clone().into();
            update.latitude = Set(site_ref.latitude);
            update.longitude = Set(site_ref.longitude);
            update.altitude_m = Set(site_ref.altitude_m);
            update.update(txn).await?;
        }
        cache.insert(key, existing.id);
        return Ok(existing.id);
    }
    let id = Uuid::new_v4();
    sites::ActiveModel {
        id: Set(id),
        project_id: Set(Some(project_id)),
        name: Set(site_ref.name.clone()),
        latitude: Set(site_ref.latitude),
        longitude: Set(site_ref.longitude),
        altitude_m: Set(site_ref.altitude_m),
        public_slug: Set(None),
        created_at: Set(Some(Utc::now())),
        discovered_at: Set(Some(Utc::now())),
    }.insert(txn).await?;
    *created_count += 1;
    cache.insert(key, id);
    Ok(id)
}

async fn resolve_or_create_param(
    txn: &impl ConnectionTrait,
    param_ref: &PlanParamRef,
    cache: &mut HashMap<String, Uuid>,
    created_count: &mut u32,
) -> AppResult<Uuid> {
    if let Some(id) = param_ref.id {
        return Ok(id);
    }
    let key = param_ref.name.to_lowercase();
    if let Some(&id) = cache.get(&key) {
        return Ok(id);
    }
    // 1. Exact name match (case-insensitive)
    let existing = parameters::Entity::find()
        .filter(Expr::cust_with_values("LOWER(name) = $1", [key.clone()]))
        .one(txn).await?;
    if let Some(existing) = existing {
        cache.insert(key, existing.id);
        return Ok(existing.id);
    }
    // 2. Alias match: check if any parameter has this name in its aliases array
    let alias_match = parameters::Entity::find()
        .filter(Expr::cust_with_values("$1 = ANY(aliases)", [param_ref.name.clone()]))
        .one(txn).await?;
    if let Some(matched) = alias_match {
        cache.insert(key, matched.id);
        return Ok(matched.id);
    }
    // 3. No match — create new with inferred category
    let category = infer_category(&param_ref.name);
    let id = Uuid::new_v4();
    parameters::ActiveModel {
        id: Set(id),
        name: Set(param_ref.name.clone()),
        display_name: Set(param_ref.name.clone()),
        default_units: Set(param_ref.units.clone()),
        category: Set(category),
        data_type: Set("numeric".to_string()),
        description: Set(None),
        aliases: Set(vec![]),
        default_warning_min: Set(None), default_warning_max: Set(None),
        default_alarm_min: Set(None), default_alarm_max: Set(None),
        created_at: Set(Some(Utc::now())),
    }.insert(txn).await?;
    *created_count += 1;
    cache.insert(key, id);
    Ok(id)
}

fn infer_category(name: &str) -> String {
    let lower = name.to_lowercase();
    if lower.contains("nitrat") || lower.contains("nitrit") || lower.contains("ammon")
        || lower.contains("phosph") || lower.contains("nitrogen") || lower.contains("nutrient")
    {
        "Nutrients".to_string()
    } else if lower.contains("peak ") || lower.contains("cdom") || lower.contains("fluor")
        || lower.contains("humif") || lower.contains("bix") || lower.contains("hix")
        || lower.contains("suva") || lower.contains("absorb") || lower.contains("e2/e3")
        || lower.contains("e4/e6") || lower.contains("slope ratio") || lower.contains("spectral")
    {
        "DOM".to_string()
    } else if lower.contains("calcium") || lower.contains("magnesium") || lower.contains("sodium")
        || lower.contains("potassium") || lower.contains("chloride") || lower.contains("sulfate")
        || lower.contains("fluoride") || lower.contains("bromide") || lower.contains("lithium")
    {
        "Ions".to_string()
    } else if lower.contains("isotop") || lower.contains("δ") || lower.contains("d-excess")
        || lower.contains("d18o") || lower.contains("d13c")
    {
        "Isotopes".to_string()
    } else if lower.contains("co2") || lower.contains("pco2") || lower.contains("methane")
        || lower.contains("ch4")
    {
        "pCO2".to_string()
    } else if lower.contains("dissolved organic carbon") || lower.contains("doc") {
        "DOC".to_string()
    } else if lower.contains("dissolved inorganic carbon") || lower.contains("dic") {
        "DIC".to_string()
    } else if lower.contains("temperatur") || lower.contains("conductiv") || lower.contains("turbid")
        || lower.contains("dissolved oxygen") || lower.contains("alkalin") || lower == "ph"
    {
        "Physicochemical".to_string()
    } else if lower.contains("depth") || lower.contains("water level") {
        "Hydrology".to_string()
    } else if lower.contains("battery") || lower.contains("signal") || lower.contains("batt") {
        "device_health".to_string()
    } else if lower.contains("suspend") || lower.contains("tss") || lower.contains("afdm") {
        "TSS".to_string()
    } else {
        "measurement".to_string()
    }
}
