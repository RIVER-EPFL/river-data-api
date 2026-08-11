use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, EntityTrait, QueryFilter,
    QueryOrder, Set, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::routes::private::{data_streams, data_streams::pairing_plans, parameters, projects, sites::parameters as site_parameters, sites};
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

    // Last resort: source_name, stripping the "{site} - " prefix without truncating
    // display names that themselves contain " - "
    let parameter = stream.source_name.as_deref()
        .and_then(|n| n.splitn(2, " - ").nth(1))
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
    #[serde(default)]
    pub original_parameter_name: Option<String>,
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
    #[serde(default)]
    pub group_key: Option<String>,
    #[serde(default)]
    pub original_names: Vec<String>,
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

struct ParamGroupProposal {
    proposed_name: String,
    units: String,
    original_names: Vec<String>,
    entry_indices: Vec<usize>,
}

fn group_streams_by_parameter(
    entries: &[(usize, String, String)],
) -> Vec<ParamGroupProposal> {
    // Distinct quantities can share a units suffix (e.g. "Nitrate [µg/L]" vs
    // "Ammonia [µg/L]"), so only entries whose names are identical group together.
    let mut by_key: HashMap<(String, String), Vec<(usize, String)>> = HashMap::new();
    for (idx, name, units) in entries {
        by_key.entry((units.to_lowercase(), name.to_lowercase()))
            .or_default()
            .push((*idx, name.clone()));
    }

    by_key.into_iter()
        .map(|((units, _), members)| {
            let mut original_names: Vec<String> = members.iter().map(|(_, n)| n.clone()).collect();
            original_names.sort();
            original_names.dedup();
            ParamGroupProposal {
                proposed_name: members[0].1.clone(),
                units,
                original_names,
                entry_indices: members.iter().map(|(idx, _)| *idx).collect(),
            }
        })
        .collect()
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

    let catalog = load_entity_catalog(db).await?;

    // Build entries
    let mut entries: Vec<PlanEntry> = Vec::with_capacity(streams.len());

    for stream in &streams {
        let h = extract_hierarchy(stream);

        let action = if h.site.is_empty() || h.parameter.is_empty() {
            "skip".to_string()
        } else {
            "pair".to_string()
        };

        let mut entry = PlanEntry {
            stream_id: stream.id,
            source_key: stream.source_key.clone(),
            source_name: stream.source_name.clone(),
            action,
            project: PlanEntityRef {
                id: None,
                name: h.project,
                create: false,
            },
            site: PlanSiteRef {
                id: None,
                name: h.site,
                create: false,
                latitude: h.latitude,
                longitude: h.longitude,
                altitude_m: h.altitude_m,
            },
            parameter: PlanParamRef {
                id: None,
                name: h.parameter.clone(),
                create: false,
                units: h.units,
                group_key: None,
                original_names: vec![],
            },
            confidence: "none".to_string(),
            warnings: vec![],
            original_parameter_name: Some(h.parameter),
        };
        reclassify_entry(&mut entry, &catalog);
        entries.push(entry);
    }

    // Group new-to-create parameters with identical names (per units) across sites
    let to_group: Vec<(usize, String, String)> = entries.iter().enumerate()
        .filter(|(_, e)| e.action == "pair" && e.parameter.create)
        .map(|(i, e)| (i, e.parameter.name.clone(), e.parameter.units.clone()))
        .collect();

    if !to_group.is_empty() {
        for group in group_streams_by_parameter(&to_group) {
            if group.entry_indices.len() <= 1 {
                continue;
            }
            let key = format!("{}::{}", group.units, group.proposed_name);
            for &idx in &group.entry_indices {
                entries[idx].parameter.name = group.proposed_name.clone();
                entries[idx].parameter.group_key = Some(key.clone());
                entries[idx].parameter.original_names = group.original_names.clone();
            }
        }
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
    #[serde(default)]
    pub streams_skipped: u32,
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
    streams_skipped: u32,
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

    // Atomic status claim: a concurrent apply of the same plan matches zero rows and bails.
    // A rollback restores 'draft'.
    let claimed = txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE pairing_plans SET status = 'applying' WHERE id = $1 AND status = 'draft'",
        [plan_id.into()],
    )).await?;
    if claimed.rows_affected() == 0 {
        return Err(AppError::BadRequest(
            "Plan is no longer in draft status".to_string(),
        ));
    }

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
        sp_created: 0, streams_paired: 0, streams_skipped: 0,
    };

    for entry in entries.iter().filter(|e| e.action == "pair") {
        if (entry.site.id.is_none() && entry.site.name.trim().is_empty())
            || (entry.parameter.id.is_none() && entry.parameter.name.trim().is_empty())
        {
            tracing::warn!(
                stream_id = %entry.stream_id,
                "apply_plan: skipping entry with empty site or parameter name",
            );
            counters.streams_skipped += 1;
            continue;
        }
        let Some(stream) = data_streams::Entity::find_by_id(entry.stream_id).one(&txn).await? else {
            tracing::warn!(
                stream_id = %entry.stream_id,
                "apply_plan: skipping entry whose stream no longer exists",
            );
            counters.streams_skipped += 1;
            continue;
        };
        let (site_parameter_id, parameter_id) = resolve_plan_entry(
            &txn, entry, &plan.source_system, &mut caches, &mut counters,
        ).await?;
        if let Some(existing_sp) = stream.site_parameter_id
            && existing_sp != site_parameter_id
        {
            tracing::warn!(
                stream_id = %entry.stream_id,
                site_parameter_id = %existing_sp,
                "apply_plan: skipping stream already paired to a different site_parameter",
            );
            counters.streams_skipped += 1;
            continue;
        }
        pair_entry_stream(&txn, stream, plan_id, site_parameter_id, parameter_id).await?;
        counters.streams_paired += 1;
    }

    let readings_backfilled = backfill_plan_readings(&txn, plan_id).await?;
    finalize_plan(&txn, plan_id, &counters, readings_backfilled).await?;
    txn.commit().await?;

    // Re-derive the paired readings by the deployment + calibration windows for each touched
    // (site, parameter) slot, then a full refresh as a safety net. `backfill_plan_readings` only
    // stamps site_id/parameter_id; the window-aware engine (same one ingest/reprocess use) assigns
    // sensor_id/deployment_id/calibration_id and the per-window calibrated_value, while its recall
    // guard leaves pre-deployment history attributed by the pairing. Runs post-commit because the
    // reprocess opens its own transaction and refreshes continuous aggregates (which can't run
    // inside one).
    let slot_rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT DISTINCT sp.site_id, sp.parameter_id
              FROM data_streams ds JOIN site_parameters sp ON ds.site_parameter_id = sp.id
              WHERE ds.pairing_plan_id = $1",
            [plan_id.into()],
        ))
        .await
        .unwrap_or_default();
    let slots: Vec<(Uuid, Uuid)> = slot_rows
        .into_iter()
        .filter_map(|r| {
            let s: Uuid = r.try_get("", "site_id").ok()?;
            let p: Uuid = r.try_get("", "parameter_id").ok()?;
            Some((s, p))
        })
        .collect();
    let db_clone = db.clone();
    tokio::spawn(async move {
        for (site_id, parameter_id) in slots {
            if let Err(e) = crate::routes::private::sensors::calibrations::service::reprocess_site_parameter_readings(
                &db_clone, site_id, parameter_id,
            )
            .await
            {
                tracing::warn!(error = %e, %site_id, %parameter_id, "apply_plan: slot reprocess failed");
            }
        }
        crate::common::sync_state::refresh_continuous_aggregates_full(&db_clone).await;
    });

    let result = ApplyResult {
        projects_created: counters.projects_created,
        sites_created: counters.sites_created,
        parameters_created: counters.params_created,
        site_parameters_created: counters.sp_created,
        streams_paired: counters.streams_paired,
        streams_skipped: counters.streams_skipped,
        readings_backfilled,
    };

    tracing::info!(
        plan_id = %plan_id,
        streams_paired = counters.streams_paired,
        streams_skipped = counters.streams_skipped,
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
        txn, &entry.parameter, entry.original_parameter_name.as_deref(),
        &mut caches.params, &mut caches.param_names, &mut counters.params_created,
    ).await?;
    let site_parameter_id = resolve_or_create_site_param(
        txn, site_id, parameter_id, &entry.parameter.units, &caches.param_names,
        &mut caches.site_params, &mut counters.sp_created,
    ).await?;
    Ok((site_parameter_id, parameter_id))
}

async fn resolve_or_create_site_param<C: ConnectionTrait>(
    txn: &C,
    site_id: Uuid,
    parameter_id: Uuid,
    units: &str,
    param_names: &HashMap<Uuid, String>,
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
        let mut param_name_val = param_names.get(&parameter_id).cloned().unwrap_or_default();
        // (site_id, name) is unique; a clash here means the name belongs to a different
        // parameter's slot, so suffix with units (or the parameter code) to disambiguate.
        let name_taken = site_parameters::Entity::find()
            .filter(Condition::all()
                .add(site_parameters::Column::SiteId.eq(site_id))
                .add(site_parameters::Column::Name.eq(param_name_val.clone())))
            .one(txn).await?
            .is_some();
        if name_taken {
            let suffix = if !units.trim().is_empty() {
                units.trim().to_string()
            } else {
                parameters::Entity::find_by_id(parameter_id)
                    .one(txn).await?
                    .map(|p| p.code)
                    .unwrap_or_else(|| parameter_id.to_string())
            };
            param_name_val = format!("{param_name_val} ({suffix})");
        }
        let units_val = {
            let u = units.trim();
            (!u.is_empty()).then(|| u.to_string())
        };
        site_parameters::ActiveModel {
            id: Set(id),
            site_id: Set(site_id),
            parameter_id: Set(parameter_id),
            name: Set(param_name_val),
            sensor_type: Set(String::new()),
            display_units: Set(units_val.clone()), units_name: Set(units_val),
            units_min: Set(None), units_max: Set(None),
            decimal_places: Set(None), channel_id: Set(None),
            sample_interval_sec: Set(None),
            is_active: Set(Some(true)),
            is_public: Set(Some(false)),
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
    stream: data_streams::Model,
    plan_id: Uuid,
    site_parameter_id: Uuid,
    parameter_id: Uuid,
) -> AppResult<()> {
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

    // Replicate groups on the newly paired streams (2+ readings sharing a stream+timestamp, e.g.
    // migrated NOMIS A/B/C rows mapped to replicate_index 0/1/2) form samples: find-or-create the
    // samples row per group, then stamp sample_id. The row-level triggers populate the statistics.
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"INSERT INTO samples (site_id, parameter_id, collected_at)
          SELECT r.site_id, r.parameter_id, r.time
          FROM readings r
          JOIN data_streams ds ON r.stream_id = ds.id
          WHERE ds.pairing_plan_id = $1 AND r.sample_id IS NULL AND r.site_id IS NOT NULL
          GROUP BY r.stream_id, r.site_id, r.parameter_id, r.time
          HAVING COUNT(*) >= 2
          ON CONFLICT (site_id, parameter_id, collected_at) DO NOTHING",
        [plan_id.into()],
    )).await?;
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE readings r
          SET sample_id = s.id
          FROM (
              SELECT r2.stream_id, r2.time
              FROM readings r2
              JOIN data_streams ds ON r2.stream_id = ds.id
              WHERE ds.pairing_plan_id = $1 AND r2.sample_id IS NULL AND r2.site_id IS NOT NULL
              GROUP BY r2.stream_id, r2.time
              HAVING COUNT(*) >= 2
          ) g, samples s
          WHERE r.stream_id = g.stream_id AND r.time = g.time AND r.sample_id IS NULL
            AND s.site_id = r.site_id AND s.parameter_id = r.parameter_id
            AND s.collected_at = r.time",
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
        streams_skipped: counters.streams_skipped,
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

    // Atomic status claim: a concurrent revert of the same plan matches zero rows and bails.
    let claimed = txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE pairing_plans SET status = 'reverting' WHERE id = $1 AND status = 'applied'",
        [plan_id.into()],
    )).await?;
    if claimed.rows_affected() == 0 {
        return Err(AppError::BadRequest(
            "Plan is no longer in applied status".to_string(),
        ));
    }

    txn.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0".to_owned(),
    )).await?;

    // NULL out readings for streams from this plan; samples formed by the pairing backfill
    // lose their last reference and are removed below
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE readings r SET site_id = NULL, parameter_id = NULL, sample_id = NULL
          FROM data_streams ds
          WHERE r.stream_id = ds.id AND ds.pairing_plan_id = $1",
        [plan_id.into()],
    )).await?;

    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"DELETE FROM samples s
          WHERE NOT EXISTS (SELECT 1 FROM readings r WHERE r.sample_id = s.id)",
        Vec::<sea_orm::Value>::new(),
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

    // Unpair the streams; pairing_plan_id stays as the audit link back to this plan
    let result = txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE data_streams SET site_parameter_id = NULL, paired_at = NULL
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

// Same resolution order as `resolve_or_create_param` uses at apply: code, then name, then alias,
// all case-insensitive, so the review shows the result apply will produce.
fn match_entity_display(name: &str, existing: &[CatalogParam]) -> (Option<Uuid>, bool) {
    if name.is_empty() {
        return (None, false);
    }
    let lower = name.to_lowercase();
    let matched = existing.iter().find(|p| p.code.to_lowercase() == lower)
        .or_else(|| existing.iter().find(|p| p.name.to_lowercase() == lower))
        .or_else(|| existing.iter().find(|p| p.aliases.iter().any(|a| a.to_lowercase() == lower)));
    match matched {
        Some(p) => (Some(p.id), false),
        None => (None, true),
    }
}

pub struct CatalogParam {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub units: String,
}

pub struct EntityCatalog {
    pub projects: Vec<(Uuid, String)>,
    pub sites: Vec<(Uuid, String)>,
    pub params: Vec<CatalogParam>,
}

pub async fn load_entity_catalog(db: &impl ConnectionTrait) -> AppResult<EntityCatalog> {
    let projects = projects::Entity::find()
        .all(db).await?
        .into_iter().map(|p| (p.id, p.name)).collect();
    let sites = sites::Entity::find()
        .all(db).await?
        .into_iter().map(|s| (s.id, s.name)).collect();
    let params = parameters::Entity::find()
        .all(db).await?
        .into_iter().map(|p| CatalogParam {
            id: p.id,
            code: p.code,
            name: p.name,
            aliases: p.aliases,
            units: p.default_units,
        })
        .collect();
    Ok(EntityCatalog { projects, sites, params })
}

/// Recompute an entry's entity resolution against the current catalog: project/site/parameter
/// id + create flags, unit-mismatch warnings, and overall confidence. Warnings are rebuilt from
/// scratch so ones that no longer apply are cleared. Does not touch action or grouping fields.
pub fn reclassify_entry(entry: &mut PlanEntry, catalog: &EntityCatalog) {
    let (proj_id, proj_create) = match_entity(&entry.project.name, &catalog.projects);
    entry.project.id = proj_id;
    entry.project.create = proj_create;

    let (site_id, site_create) = match_entity(&entry.site.name, &catalog.sites);
    entry.site.id = site_id;
    entry.site.create = site_create;

    let (param_id, param_create) = match_entity_display(&entry.parameter.name, &catalog.params);
    entry.parameter.id = param_id;
    entry.parameter.create = param_create;

    entry.warnings.clear();
    if let Some(pid) = param_id
        && let Some(p) = catalog.params.iter().find(|p| p.id == pid)
        && !p.units.is_empty() && !entry.parameter.units.is_empty()
        && p.units.to_lowercase() != entry.parameter.units.to_lowercase()
    {
        entry.warnings.push(format!(
            "Parameter '{}' exists with units '{}' but this source uses '{}'",
            entry.parameter.name, p.units, entry.parameter.units
        ));
    }

    entry.confidence = if proj_id.is_some() && site_id.is_some() && param_id.is_some() {
        "exact"
    } else {
        "none"
    }.to_string();
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
        public_code: Set(None),
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
        // The site was matched at plan-creation time. Still backfill coordinates from the stream
        // metadata if the site lacks them — otherwise a site discovered before its coordinates were
        // known never picks them up (the common case, since match_entity sets the id).
        if site_ref.latitude.is_some()
            && let Some(existing) = sites::Entity::find_by_id(id).one(txn).await?
            && existing.latitude.is_none()
        {
            let mut update: sites::ActiveModel = existing.into();
            update.latitude = Set(site_ref.latitude);
            update.longitude = Set(site_ref.longitude);
            update.altitude_m = Set(site_ref.altitude_m);
            update.update(txn).await?;
        }
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
        subproject_id: sea_orm::ActiveValue::NotSet,
        name: Set(site_ref.name.clone()),
        latitude: Set(site_ref.latitude),
        longitude: Set(site_ref.longitude),
        altitude_m: Set(site_ref.altitude_m),
        public_code: Set(None),
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
    original_parameter_name: Option<&str>,
    cache: &mut HashMap<String, Uuid>,
    param_names: &mut HashMap<Uuid, String>,
    created_count: &mut u32,
) -> AppResult<Uuid> {
    if let Some(id) = param_ref.id {
        return Ok(id);
    }
    let key = param_ref.name.to_lowercase();
    if let Some(&id) = cache.get(&key) {
        return Ok(id);
    }
    // Resolution order mirrors `match_entity_display`: code, then name, then alias,
    // all case-insensitive.
    let existing = parameters::Entity::find()
        .filter(Expr::cust_with_values("LOWER(code) = $1", [key.clone()]))
        .one(txn).await?;
    if let Some(existing) = existing {
        cache.insert(key, existing.id);
        param_names.entry(existing.id).or_insert(existing.name);
        return Ok(existing.id);
    }
    let name_match = parameters::Entity::find()
        .filter(Expr::cust_with_values("LOWER(name) = $1", [key.clone()]))
        .one(txn).await?;
    if let Some(matched) = name_match {
        cache.insert(key, matched.id);
        param_names.entry(matched.id).or_insert(matched.name);
        return Ok(matched.id);
    }
    let alias_match = parameters::Entity::find()
        .filter(Expr::cust_with_values(
            "EXISTS (SELECT 1 FROM unnest(aliases) a WHERE LOWER(a) = $1)",
            [key.clone()],
        ))
        .one(txn).await?;
    if let Some(matched) = alias_match {
        cache.insert(key, matched.id);
        param_names.entry(matched.id).or_insert(matched.name);
        return Ok(matched.id);
    }
    // No match: create, seeding aliases from the source names so future plans resolve them
    let mut aliases: Vec<String> = param_ref.original_names.iter().cloned()
        .chain(original_parameter_name.map(str::to_string))
        .filter(|a| !a.trim().is_empty() && a.to_lowercase() != key)
        .collect();
    aliases.sort();
    aliases.dedup_by(|a, b| a.to_lowercase() == b.to_lowercase());
    let category = infer_category(&param_ref.name);
    let id = Uuid::new_v4();
    parameters::ActiveModel {
        id: Set(id),
        code: Set(param_ref.name.clone()),
        name: Set(param_ref.name.clone()),
        default_units: Set(param_ref.units.clone()),
        category: Set(category),
        description: Set(None),
        aliases: Set(aliases),
        default_warning_min: Set(None), default_warning_max: Set(None),
        default_alarm_min: Set(None), default_alarm_max: Set(None),
        created_at: Set(Some(Utc::now())),
    }.insert(txn).await?;
    *created_count += 1;
    cache.insert(key, id);
    param_names.insert(id, param_ref.name.clone());
    Ok(id)
}

fn infer_category(_name: &str) -> String {
    "measurement".to_string()
}
