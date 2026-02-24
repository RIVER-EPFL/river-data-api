use chrono::{Duration, Utc};
use sea_orm::{ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter, Set, Statement};
use std::collections::HashMap;
use uuid::Uuid;

use crate::entity::{alarm_thresholds, device_status, parameters, projects, readings, sites, sync_state};
use crate::error::AppResult;
use crate::vaisala::VaisalaClient;

/// Batch size for bulk inserts
const BATCH_SIZE: usize = 1000;

/// Discover and sync projects, sites, and parameters from Vaisala.
///
/// Parses the location hierarchy from Vaisala's `/locations` endpoint and creates
/// any missing projects, sites, or parameters in the database.
///
/// Hierarchy (based on path depth):
/// - viewLinc (root, ignored)
///   - Project (depth 1, e.g., "BREATHE")
///     - Site (depth 2, e.g., "Martigny")
///       - Parameter (depth 3, leaf=true, e.g., "MDepthmm")
pub async fn sync_locations(db: &DatabaseConnection, vaisala: &VaisalaClient) -> AppResult<()> {
    tracing::info!("Discovering locations from Vaisala...");

    let locations = vaisala.get_locations().await?;

    let now = Utc::now();

    // Build maps of existing entities by their source identifiers
    let existing_projects: HashMap<String, projects::Model> = projects::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|p| (p.name.clone(), p))
        .collect();

    let existing_sites: HashMap<i32, sites::Model> = sites::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.source_node_id, s))
        .collect();

    let existing_params: HashMap<i32, parameters::Model> = parameters::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|p| (p.source_location_id, p))
        .collect();

    let mut projects_created = 0;
    let mut sites_created = 0;
    let mut params_created = 0;

    // Maps to track newly created projects/sites by name for FK lookups
    let mut project_ids: HashMap<String, Uuid> = existing_projects
        .iter()
        .map(|(name, p)| (name.clone(), p.id))
        .collect();
    let mut site_ids: HashMap<i32, Uuid> = existing_sites
        .iter()
        .map(|(node_id, s)| (*node_id, s.id))
        .collect();

    let mut new_param_location_ids: Vec<i32> = Vec::new();

    for resource in &locations.data {
        let attrs = &resource.attributes;

        if attrs.deleted {
            continue;
        }

        let parts: Vec<&str> = attrs.path.split('/').collect();

        match (parts.len(), attrs.leaf) {
            // Project: path like "viewLinc/BREATHE" (2 parts, not leaf)
            (2, false) => {
                let project_name = parts[1];
                if !project_ids.contains_key(project_name) {
                    let project = projects::ActiveModel {
                        id: Set(Uuid::new_v4()),
                        name: Set(project_name.to_string()),
                        source_path: Set(Some(attrs.path.clone())),
                        description: Set(if attrs.description.is_empty() {
                            None
                        } else {
                            Some(attrs.description.clone())
                        }),
                        created_at: Set(Some(now.into())),
                        discovered_at: Set(Some(now.into())),
                    };

                    match project.insert(db).await {
                        Ok(p) => {
                            project_ids.insert(project_name.to_string(), p.id);
                            projects_created += 1;
                            tracing::debug!(name = project_name, "Created project");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, name = project_name, "Failed to create project");
                        }
                    }
                }
            }

            // Site: path like "viewLinc/BREATHE/Martigny" (3 parts, not leaf)
            (3, false) => {
                let project_name = parts[1];
                let site_name = parts[2];

                if !site_ids.contains_key(&attrs.node_id) {
                    let project_id = project_ids.get(project_name).copied();

                    let site = sites::ActiveModel {
                        id: Set(Uuid::new_v4()),
                        project_id: Set(project_id),
                        name: Set(site_name.to_string()),
                        source_node_id: Set(attrs.node_id),
                        source_path: Set(Some(attrs.path.clone())),
                        latitude: Set(None),
                        longitude: Set(None),
                        altitude_m: Set(None),
                        created_at: Set(Some(now.into())),
                        discovered_at: Set(Some(now.into())),
                    };

                    match site.insert(db).await {
                        Ok(s) => {
                            site_ids.insert(attrs.node_id, s.id);
                            sites_created += 1;
                            tracing::debug!(name = site_name, node_id = attrs.node_id, "Created site");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, name = site_name, "Failed to create site");
                        }
                    }
                }
            }

            // Parameter: leaf=true with path like "viewLinc/BREATHE/Martigny/MDepthmm"
            (_, true) if parts.len() >= 4 => {
                if !existing_params.contains_key(&attrs.node_id) {
                    new_param_location_ids.push(attrs.node_id);
                }
            }

            _ => {
                // Other hierarchy depths or patterns - skip
            }
        }
    }

    // Fetch detailed info for new parameters and create them
    if !new_param_location_ids.is_empty() {
        tracing::debug!(count = new_param_location_ids.len(), "Fetching parameter details");

        let param_data = vaisala.get_locations_data(&new_param_location_ids).await?;

        for resource in param_data.data {
            let attrs = resource.attributes;

            let parts: Vec<&str> = attrs.location_path.split('/').collect();
            if parts.len() < 4 {
                continue;
            }

            let site_path = parts[..3].join("/");

            let site_node_id = locations
                .data
                .iter()
                .find(|r| r.attributes.path == site_path)
                .map(|r| r.attributes.node_id);

            let Some(site_id) = site_node_id.and_then(|nid| site_ids.get(&nid).copied()) else {
                tracing::warn!(
                    location_id = attrs.id,
                    path = attrs.location_path,
                    "Could not find site for parameter"
                );
                continue;
            };

            // Derive sensor_type from the Vaisala name (e.g., "MDepthmm" -> "Depth")
            let sensor_type = derive_parameter_type(&attrs.location_name);
            let sensor_type_for_threshold = sensor_type.clone();

            // Use generic name (strip station prefix)
            let generic_name = derive_generic_name(&attrs.location_name);

            let param = parameters::ActiveModel {
                id: Set(Uuid::new_v4()),
                site_id: Set(site_id),
                source_location_id: Set(attrs.id),
                name: Set(generic_name.clone()),
                sensor_type: Set(sensor_type),
                display_units: Set(Some(attrs.display_units.clone())),
                units_name: Set(None),
                units_min: Set(None),
                units_max: Set(None),
                decimal_places: Set(Some(attrs.decimal_places)),
                device_serial_number: Set(if attrs.logger_serial_number.is_empty() {
                    None
                } else {
                    Some(attrs.logger_serial_number.clone())
                }),
                probe_serial_number: Set(if attrs.probe_serial_number.is_empty() {
                    None
                } else {
                    Some(attrs.probe_serial_number.clone())
                }),
                channel_id: Set(if attrs.channel_id == 0 {
                    None
                } else {
                    Some(attrs.channel_id)
                }),
                sample_interval_sec: Set(if attrs.sample_interval_sec == 0 {
                    None
                } else {
                    Some(attrs.sample_interval_sec)
                }),
                is_active: Set(Some(true)),
                created_at: Set(Some(now.into())),
                updated_at: Set(Some(now.into())),
                discovered_at: Set(Some(now.into())),
            };

            match param.insert(db).await {
                Ok(p) => {
                    // Initialize sync_state for the new parameter
                    let sync = sync_state::ActiveModel {
                        parameter_id: Set(p.id),
                        last_data_time: Set(None),
                        last_sync_attempt: Set(None),
                        sync_status: Set(Some("pending".to_string())),
                        error_message: Set(None),
                        retry_count: Set(Some(0)),
                        last_full_sync: Set(None),
                    };
                    let _ = sync.insert(db).await;

                    // Create alarm threshold based on sensor_type
                    let threshold = create_threshold_for_sensor_type(&p.id, &sensor_type_for_threshold);
                    if let Err(e) = threshold.insert(db).await {
                        tracing::warn!(
                            error = %e,
                            parameter_id = %p.id,
                            "Failed to create alarm threshold"
                        );
                    }

                    params_created += 1;
                    tracing::debug!(
                        name = generic_name,
                        vaisala_name = attrs.location_name,
                        location_id = attrs.id,
                        "Created parameter"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        name = attrs.location_name,
                        "Failed to create parameter"
                    );
                }
            }
        }
    }

    tracing::info!(
        projects = projects_created,
        sites = sites_created,
        parameters = params_created,
        "Location discovery complete"
    );

    Ok(())
}

/// Derive sensor type from the Vaisala sensor name.
/// E.g., "MDepthmm" -> "Depth", "MCDOMppb" -> "CDOM"
/// Handles both old Vaisala names (with station prefix) and new generic names.
fn derive_parameter_type(name: &str) -> String {
    let patterns: &[(&str, &[&str])] = &[
        ("Depth", &["depth", "Depth", "WaterDepth"]),
        ("CDOM", &["cdom", "CDOM"]),
        ("Turbidity", &["turb", "Turb", "Turbi"]),
        ("Battery", &["batt", "Batt"]),
        ("DO_Temperature", &["DOdegC", "DOTdegC", "WaterTemp"]),
        ("Dissolved_O2", &["DOuM"]),
        ("Conductivity", &["Condu", "condu"]),
        ("Cond_Temperature", &["CondT"]),
    ];

    for (sensor_type, keywords) in patterns {
        for keyword in *keywords {
            if name.contains(keyword) {
                return (*sensor_type).to_string();
            }
        }
    }

    // Default: use the name itself
    name.to_string()
}

/// Convert a Vaisala sensor name to a generic name by stripping the station prefix.
/// E.g., "MDepthmm" -> "WaterDepthmm", "MCDOMppb" -> "CDOMppb", "DDOuM" -> "DOuM"
fn derive_generic_name(vaisala_name: &str) -> String {
    let mappings: &[(&str, &str)] = &[
        // Depth
        ("Depthmm", "WaterDepthmm"),
        // CDOM
        ("CDOMppb", "CDOMppb"),
        // Turbidity
        ("TurbNTU", "TurbiNTU"),
        // Battery
        ("BattV", "BattV"),
        // DO Temperature (two variants from Vaisala)
        ("DOdegC", "WaterTempdegC"),
        ("DOTdegC", "WaterTempdegC"),
        // Dissolved O2
        ("DOuM", "DOuM"),
        // Conductivity (two case variants)
        ("ConduSCm", "ConduScm"),
        ("ConduScm", "ConduScm"),
        // Conductivity Temperature
        ("CondTdegC", "CondTempdegC"),
    ];

    // Try to match by stripping the first character (station prefix)
    if vaisala_name.len() > 1 {
        let without_prefix = &vaisala_name[1..];
        for (suffix, generic) in mappings {
            if without_prefix == *suffix || without_prefix.ends_with(suffix) {
                return (*generic).to_string();
            }
        }
    }

    // If no mapping found, return as-is
    vaisala_name.to_string()
}

/// Sync readings for all active parameters.
pub async fn sync_readings(
    db: &DatabaseConnection,
    vaisala: &VaisalaClient,
    max_history_days: i64,
    force_full_sync: bool,
) -> AppResult<()> {
    let params_with_state: Vec<(parameters::Model, Option<sync_state::Model>)> =
        parameters::Entity::find()
            .filter(parameters::Column::IsActive.eq(true))
            .find_also_related(sync_state::Entity)
            .all(db)
            .await?;

    if params_with_state.is_empty() {
        tracing::debug!("No active parameters to sync");
        return Ok(());
    }

    // Build a map of source_location_id -> (parameter_id, last_data_time)
    let mut location_map: HashMap<i32, (Uuid, Option<chrono::DateTime<Utc>>)> = HashMap::new();
    for (param, state) in &params_with_state {
        let last_time = if force_full_sync {
            None
        } else {
            state
                .as_ref()
                .and_then(|s| s.last_data_time.map(|dt| dt.with_timezone(&Utc)))
        };
        location_map.insert(param.source_location_id, (param.id, last_time));
    }

    let now = Utc::now();
    let max_history_start = now - Duration::days(max_history_days);

    let location_ids: Vec<i32> = location_map.keys().copied().collect();

    let earliest_from = location_map
        .values()
        .map(|(_, last_time)| last_time.unwrap_or(max_history_start))
        .min()
        .unwrap_or(max_history_start);

    tracing::info!(
        parameter_count = location_ids.len(),
        from = %earliest_from,
        "Syncing readings"
    );

    let history = match vaisala
        .get_locations_history(&location_ids, earliest_from, Some(now))
        .await
    {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "Failed to fetch locations history");
            for (param, _) in &params_with_state {
                update_sync_state_error(db, param.id, &e.to_string()).await;
            }
            return Err(e);
        }
    };

    for resource in history.data {
        let attrs = resource.attributes;
        let Some((parameter_id, last_time)) = location_map.get(&attrs.id) else {
            tracing::warn!(
                location_id = attrs.id,
                "Received data for unknown location"
            );
            continue;
        };

        let last_timestamp = last_time.map(|lt| lt.timestamp());
        let new_points: Vec<_> = attrs
            .data_points
            .into_iter()
            .filter(|dp| last_timestamp.is_none_or(|lt| dp.timestamp > lt))
            .collect();

        if new_points.is_empty() {
            tracing::debug!(
                parameter_id = %parameter_id,
                location_id = attrs.id,
                "No new samples"
            );
            continue;
        }

        let sample_count = new_points.len();

        let mut models: Vec<readings::ActiveModel> = Vec::with_capacity(new_points.len());
        let mut latest_timestamp: Option<i64> = None;

        for point in new_points {
            let raw_time = chrono::DateTime::from_timestamp(point.timestamp, 0)
                .unwrap_or_else(Utc::now);
            let epoch = raw_time.timestamp();
            let rounded_epoch = ((epoch + 300) / 600) * 600;
            let time = chrono::DateTime::from_timestamp(rounded_epoch, 0)
                .unwrap_or(raw_time);

            models.push(readings::ActiveModel {
                parameter_id: Set(*parameter_id),
                time: Set(time.into()),
                value: Set(point.value),
                logged: Set(Some(point.logged)),
            });

            if latest_timestamp.is_none_or(|lt| point.timestamp > lt) {
                latest_timestamp = Some(point.timestamp);
            }
        }

        for chunk in models.chunks(BATCH_SIZE) {
            if let Err(e) = readings::Entity::insert_many(chunk.to_vec())
                .on_conflict(
                    sea_orm::sea_query::OnConflict::columns([
                        readings::Column::ParameterId,
                        readings::Column::Time,
                    ])
                    .do_nothing()
                    .to_owned(),
                )
                .exec(db)
                .await
            {
                let msg = e.to_string();
                if !msg.contains("None of the records") && !msg.contains("duplicate") {
                    tracing::warn!(
                        error = %e,
                        batch_size = chunk.len(),
                        "Failed to insert reading batch"
                    );
                }
            }
        }

        if let Some(ts) = latest_timestamp
            && let Some(latest) = chrono::DateTime::from_timestamp(ts, 0)
        {
            update_sync_state_success(db, *parameter_id, latest).await;
        }

        tracing::info!(
            count = sample_count,
            parameter_id = %parameter_id,
            location_id = attrs.id,
            "Synced readings"
        );
    }

    Ok(())
}

/// Sync device status for all active parameters.
pub async fn sync_device_status(db: &DatabaseConnection, vaisala: &VaisalaClient) -> AppResult<()> {
    let params: Vec<parameters::Model> = parameters::Entity::find()
        .filter(parameters::Column::IsActive.eq(true))
        .all(db)
        .await?;

    if params.is_empty() {
        tracing::debug!("No active parameters for device status sync");
        return Ok(());
    }

    let location_map: HashMap<i32, Uuid> = params
        .iter()
        .map(|p| (p.source_location_id, p.id))
        .collect();

    let location_ids: Vec<i32> = location_map.keys().copied().collect();

    tracing::info!(parameter_count = location_ids.len(), "Syncing device status");

    let data = vaisala.get_locations_data(&location_ids).await?;

    let now = Utc::now();

    for resource in data.data {
        let attrs = resource.attributes;
        let Some(parameter_id) = location_map.get(&attrs.id) else {
            continue;
        };

        let status = device_status::ActiveModel {
            parameter_id: Set(*parameter_id),
            time: Set(now.into()),
            battery_level: Set(Some(attrs.battery_level)),
            battery_state: Set(Some(attrs.battery_state)),
            signal_quality: Set(Some(attrs.signal_quality)),
            device_status: Set(Some(attrs.device_status)),
            unreachable: Set(Some(attrs.unreachable)),
        };

        if let Err(e) = status.insert(db).await {
            tracing::warn!(
                parameter_id = %parameter_id,
                error = %e,
                "Failed to insert device status"
            );
        }
    }

    tracing::info!("Device status sync completed");
    Ok(())
}

async fn update_sync_state_success(
    db: &DatabaseConnection,
    parameter_id: Uuid,
    latest_time: chrono::DateTime<Utc>,
) {
    let state = sync_state::ActiveModel {
        parameter_id: Set(parameter_id),
        last_data_time: Set(Some(latest_time.into())),
        last_sync_attempt: Set(Some(Utc::now().into())),
        sync_status: Set(Some("success".to_string())),
        error_message: Set(None),
        retry_count: Set(Some(0)),
        last_full_sync: sea_orm::ActiveValue::NotSet,
    };

    if let Err(e) = sync_state::Entity::insert(state)
        .on_conflict(
            sea_orm::sea_query::OnConflict::column(sync_state::Column::ParameterId)
                .update_columns([
                    sync_state::Column::LastDataTime,
                    sync_state::Column::LastSyncAttempt,
                    sync_state::Column::SyncStatus,
                    sync_state::Column::ErrorMessage,
                    sync_state::Column::RetryCount,
                ])
                .to_owned(),
        )
        .exec(db)
        .await
    {
        tracing::warn!(
            parameter_id = %parameter_id,
            error = %e,
            "Failed to update sync state"
        );
    }
}

async fn update_sync_state_error(db: &DatabaseConnection, parameter_id: Uuid, error: &str) {
    let current = sync_state::Entity::find_by_id(parameter_id)
        .one(db)
        .await
        .ok()
        .flatten();

    let retry_count = current.and_then(|s| s.retry_count).unwrap_or(0) + 1;

    let state = sync_state::ActiveModel {
        parameter_id: Set(parameter_id),
        last_data_time: Set(None),
        last_sync_attempt: Set(Some(Utc::now().into())),
        sync_status: Set(Some("error".to_string())),
        error_message: Set(Some(error.to_string())),
        retry_count: Set(Some(retry_count)),
        last_full_sync: sea_orm::ActiveValue::NotSet,
    };

    if let Err(e) = sync_state::Entity::insert(state)
        .on_conflict(
            sea_orm::sea_query::OnConflict::column(sync_state::Column::ParameterId)
                .update_columns([
                    sync_state::Column::LastSyncAttempt,
                    sync_state::Column::SyncStatus,
                    sync_state::Column::ErrorMessage,
                    sync_state::Column::RetryCount,
                ])
                .to_owned(),
        )
        .exec(db)
        .await
    {
        tracing::warn!(
            parameter_id = %parameter_id,
            error = %e,
            "Failed to update sync state error"
        );
    }
}

/// Update last_full_sync timestamp for all parameters.
pub async fn update_last_full_sync_for_all_parameters(db: &DatabaseConnection) {
    let now = Utc::now();

    let states = match sync_state::Entity::find().all(db).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to fetch sync states for full sync update");
            return;
        }
    };

    for state in states {
        let mut active: sync_state::ActiveModel = state.into();
        active.last_full_sync = Set(Some(now.into()));

        if let Err(e) = active.update(db).await {
            tracing::warn!(error = %e, "Failed to update last_full_sync");
        }
    }
}

/// Check if a full re-sync is needed (oldest last_full_sync > 24 hours ago, or never done).
pub async fn needs_full_sync(db: &DatabaseConnection) -> bool {
    let states = match sync_state::Entity::find().all(db).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to check full sync status, assuming needed");
            return true;
        }
    };

    if states.is_empty() {
        return true;
    }

    let now = Utc::now();
    let threshold = Duration::hours(24);

    for state in states {
        match state.last_full_sync {
            None => return true,
            Some(last) => {
                let last_utc = last.with_timezone(&Utc);
                if now - last_utc > threshold {
                    return true;
                }
            }
        }
    }

    false
}

/// Refresh continuous aggregates after new data is synced.
pub async fn refresh_continuous_aggregates(db: &DatabaseConnection) {
    tracing::debug!("Refreshing continuous aggregates...");

    let result = db
        .execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "CALL refresh_continuous_aggregate('readings_hourly', NOW() - INTERVAL '24 hours', NOW())".to_string(),
        ))
        .await;

    match result {
        Ok(_) => tracing::debug!("Hourly continuous aggregate refreshed"),
        Err(e) => tracing::warn!(error = %e, "Failed to refresh hourly aggregate"),
    }

    let result = db
        .execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "CALL refresh_continuous_aggregate('readings_daily', NOW() - INTERVAL '7 days', NOW())".to_string(),
        ))
        .await;

    match result {
        Ok(_) => tracing::debug!("Daily continuous aggregate refreshed"),
        Err(e) => tracing::warn!(error = %e, "Failed to refresh daily aggregate"),
    }
}

/// Refresh all continuous aggregates for the entire data range.
pub async fn refresh_continuous_aggregates_full(db: &DatabaseConnection) {
    tracing::info!("Refreshing continuous aggregates for full history...");

    let aggregates = ["readings_hourly", "readings_daily", "readings_weekly", "readings_monthly"];

    for agg in aggregates {
        let result = db
            .execute(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!("CALL refresh_continuous_aggregate('{agg}', NULL, NULL)"),
            ))
            .await;

        match result {
            Ok(_) => tracing::info!(aggregate = agg, "Continuous aggregate refreshed"),
            Err(e) => tracing::warn!(error = %e, aggregate = agg, "Failed to refresh aggregate"),
        }
    }

    tracing::info!("Full continuous aggregate refresh completed");
}

/// Create an alarm threshold record based on sensor type.
fn create_threshold_for_sensor_type(parameter_id: &Uuid, sensor_type: &str) -> alarm_thresholds::ActiveModel {
    let (warning_min, warning_max, alarm_min, alarm_max) = match sensor_type {
        "Depth" => (Some(100.0), Some(1000.0), Some(0.0), Some(2000.0)),
        "CDOM" => (None, Some(100.0), Some(0.0), Some(150.0)),
        "Turbidity" => (None, Some(100.0), Some(0.0), Some(500.0)),
        "Dissolved_O2" => (Some(120.0), Some(360.0), Some(0.0), Some(625.0)),
        "Conductivity" => (Some(100.0), Some(900.0), Some(0.0), Some(1000.0)),
        "DO_Temperature" | "Cond_Temperature" => (Some(0.5), Some(20.0), Some(0.0), Some(25.0)),
        "Battery" => (Some(12.1), None, Some(11.5), None),
        _ => (None, None, None, None),
    };

    let now = Utc::now().fixed_offset();
    alarm_thresholds::ActiveModel {
        id: Set(Uuid::new_v4()),
        parameter_id: Set(*parameter_id),
        warning_min: Set(warning_min),
        warning_max: Set(warning_max),
        alarm_min: Set(alarm_min),
        alarm_max: Set(alarm_max),
        description: Set(Some("Auto-generated from sensor type defaults".to_string())),
        created_at: Set(Some(now)),
        updated_at: Set(Some(now)),
    }
}
