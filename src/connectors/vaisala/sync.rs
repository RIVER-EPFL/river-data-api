use chrono::{Duration, Utc};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set,
};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use super::VaisalaClient;
use super::state::{update_sync_state_error, update_sync_state_success};
use crate::entity::{
    alarm_thresholds, parameters, projects, readings, sensor_calibrations, sensor_deployments,
    sensors, site_parameters, sites, source_mappings, sync_state,
};
use crate::error::AppResult;
use crate::services::calibration::recalculate_derived_at_timestamp;

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
///       - Parameter (depth 3, leaf=true, e.g., "`MDepthmm`")
pub async fn sync_locations(db: &DatabaseConnection, vaisala: &VaisalaClient) -> AppResult<()> {
    tracing::info!("Discovering locations from Vaisala...");

    let locations = vaisala.get_locations().await?;

    let now = Utc::now();

    // Load all source mappings for dedup and rename detection
    let mappings: Vec<source_mappings::Model> = source_mappings::Entity::find().all(db).await?;

    let project_mappings: HashMap<i32, (Uuid, Option<String>)> = mappings
        .iter()
        .filter(|m| m.entity_type == "project")
        .map(|m| (m.source_key, (m.entity_id, m.source_name.clone())))
        .collect();

    let site_mappings: HashMap<i32, (Uuid, Option<String>)> = mappings
        .iter()
        .filter(|m| m.entity_type == "site")
        .map(|m| (m.source_key, (m.entity_id, m.source_name.clone())))
        .collect();

    let param_mappings: HashMap<i32, (Uuid, Option<String>)> = mappings
        .iter()
        .filter(|m| m.entity_type == "site_parameter")
        .map(|m| (m.source_key, (m.entity_id, m.source_name.clone())))
        .collect();

    // Build project name -> UUID map for FK lookups when creating sites
    let mut project_ids: HashMap<String, Uuid> = projects::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|p| (p.name.clone(), p.id))
        .collect();

    // Build site source_key -> UUID map for FK lookups when creating parameters
    let mut site_ids: HashMap<i32, Uuid> = site_mappings
        .iter()
        .map(|(key, (id, _))| (*key, *id))
        .collect();

    let mut projects_created = 0;
    let mut sites_created = 0;
    let mut params_created = 0;

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

                // Check for rename
                if let Some((_, old_name)) = project_mappings.get(&attrs.node_id) {
                    if let Some(old) = old_name
                        && old != project_name
                    {
                        tracing::info!(
                            source_key = attrs.node_id,
                            old_name = old,
                            new_name = project_name,
                            "Vaisala project renamed"
                        );
                        let mapping = source_mappings::ActiveModel {
                            entity_type: Set("project".to_string()),
                            source_key: Set(attrs.node_id),
                            entity_id: sea_orm::ActiveValue::NotSet,
                            source_name: Set(Some(project_name.to_string())),
                            source_system: Set(Some("vaisala".to_string())),
                        };
                        if let Err(e) = mapping.update(db).await {
                            tracing::warn!(error = %e, "Failed to update project source_name");
                        }
                    }
                    continue;
                }

                // New project
                let id = Uuid::new_v4();
                let project = projects::ActiveModel {
                    id: Set(id),
                    name: Set(project_name.to_string()),
                    data_source: Set("vaisala".to_string()),
                    description: Set(if attrs.description.is_empty() {
                        None
                    } else {
                        Some(attrs.description.clone())
                    }),
                    created_at: Set(Some(now)),
                    discovered_at: Set(Some(now)),
                    is_public: Set(false),
                    public_slug: Set(None),
                    public_api_title: Set(None),
                    public_api_description: Set(None),
                    public_api_version: Set(None),
                    public_contact_email: Set(None),
                };

                match project.insert(db).await {
                    Ok(p) => {
                        // Create source mapping
                        let mapping = source_mappings::ActiveModel {
                            entity_type: Set("project".to_string()),
                            source_key: Set(attrs.node_id),
                            entity_id: Set(p.id),
                            source_name: Set(Some(project_name.to_string())),
                            source_system: Set(Some("vaisala".to_string())),
                        };
                        if let Err(e) = mapping.insert(db).await {
                            tracing::warn!(error = %e, "Failed to create project source mapping");
                        }

                        project_ids.insert(project_name.to_string(), p.id);
                        projects_created += 1;
                        tracing::debug!(name = project_name, "Created project");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, name = project_name, "Failed to create project");
                    }
                }
            }

            // Site: path like "viewLinc/BREATHE/Martigny" (3 parts, not leaf)
            (3, false) => {
                let project_name = parts[1];
                let site_name = parts[2];

                // Check for rename
                if let Some((_, old_name)) = site_mappings.get(&attrs.node_id) {
                    if let Some(old) = old_name
                        && old != site_name
                    {
                        tracing::info!(
                            source_key = attrs.node_id,
                            old_name = old,
                            new_name = site_name,
                            "Vaisala site renamed"
                        );
                        let mapping = source_mappings::ActiveModel {
                            entity_type: Set("site".to_string()),
                            source_key: Set(attrs.node_id),
                            entity_id: sea_orm::ActiveValue::NotSet,
                            source_name: Set(Some(site_name.to_string())),
                            source_system: Set(Some("vaisala".to_string())),
                        };
                        if let Err(e) = mapping.update(db).await {
                            tracing::warn!(error = %e, "Failed to update site source_name");
                        }
                    }
                    continue;
                }

                // New site
                let project_id = project_ids.get(project_name).copied();
                let id = Uuid::new_v4();

                let site = sites::ActiveModel {
                    id: Set(id),
                    project_id: Set(project_id),
                    name: Set(site_name.to_string()),
                    latitude: Set(None),
                    longitude: Set(None),
                    altitude_m: Set(None),
                    created_at: Set(Some(now)),
                    discovered_at: Set(Some(now)),
                    public_slug: Set(None),
                };

                match site.insert(db).await {
                    Ok(s) => {
                        let mapping = source_mappings::ActiveModel {
                            entity_type: Set("site".to_string()),
                            source_key: Set(attrs.node_id),
                            entity_id: Set(s.id),
                            source_name: Set(Some(site_name.to_string())),
                            source_system: Set(Some("vaisala".to_string())),
                        };
                        if let Err(e) = mapping.insert(db).await {
                            tracing::warn!(error = %e, "Failed to create site source mapping");
                        }

                        site_ids.insert(attrs.node_id, s.id);
                        sites_created += 1;
                        tracing::debug!(name = site_name, node_id = attrs.node_id, "Created site");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, name = site_name, "Failed to create site");
                    }
                }
            }

            // Parameter: leaf=true with path like "viewLinc/BREATHE/Martigny/MDepthmm"
            (_, true) if parts.len() >= 4 => {
                if !param_mappings.contains_key(&attrs.node_id) {
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
        tracing::debug!(
            count = new_param_location_ids.len(),
            "Fetching parameter details"
        );

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

            // Ensure global parameter exists for this sensor_type
            let global_param_id = get_or_create_parameter(db, &sensor_type).await;

            let param_id = Uuid::new_v4();
            let param = site_parameters::ActiveModel {
                id: Set(param_id),
                site_id: Set(site_id),
                parameter_id: Set(global_param_id),
                name: Set(generic_name.clone()),
                sensor_type: Set(sensor_type.clone()),
                display_units: Set(Some(attrs.display_units.clone())),
                units_name: Set(None),
                units_min: Set(None),
                units_max: Set(None),
                decimal_places: Set(Some(attrs.decimal_places)),
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
                is_derived: Set(Some(false)),
                derived_definition_id: Set(None),
                variable_mappings: Set(None),
                created_at: Set(Some(now)),
                updated_at: Set(Some(now)),
                discovered_at: Set(Some(now)),
            };

            match param.insert(db).await {
                Ok(p) => {
                    // Create source mapping
                    let mapping = source_mappings::ActiveModel {
                        entity_type: Set("site_parameter".to_string()),
                        source_key: Set(attrs.id),
                        entity_id: Set(p.id),
                        source_name: Set(Some(attrs.location_name.clone())),
                        source_system: Set(Some("vaisala".to_string())),
                    };
                    if let Err(e) = mapping.insert(db).await {
                        tracing::warn!(error = %e, "Failed to create site_parameter source mapping");
                    }

                    // Initialize sync_state for the new site_parameter
                    let sync = sync_state::ActiveModel {
                        site_parameter_id: Set(p.id),
                        last_data_time: Set(None),
                        last_sync_attempt: Set(None),
                        sync_status: Set(Some("pending".to_string())),
                        error_message: Set(None),
                        retry_count: Set(Some(0)),
                        last_full_sync: Set(None),
                    };
                    if let Err(e) = sync.insert(db).await {
                        tracing::warn!(error = %e, site_parameter_id = %p.id, "Failed to initialize sync state");
                    }

                    // Create alarm threshold based on sensor_type
                    let threshold = create_threshold_for_sensor_type(
                        global_param_id,
                        site_id,
                        &sensor_type_for_threshold,
                    );
                    if let Err(e) = threshold.insert(db).await {
                        tracing::warn!(
                            error = %e,
                            parameter_id = %global_param_id,
                            site_id = %site_id,
                            "Failed to create alarm threshold"
                        );
                    }

                    // Create sensor entity from Vaisala device serial number
                    // (must happen after site_parameter insert due to FK constraint)
                    if !attrs.logger_serial_number.is_empty() {
                        create_sensor_and_deployment(
                            db,
                            &attrs.logger_serial_number,
                            global_param_id,
                            site_id,
                            now,
                        )
                        .await;
                    }

                    params_created += 1;
                    tracing::debug!(
                        name = generic_name,
                        vaisala_name = attrs.location_name,
                        location_id = attrs.id,
                        "Created site_parameter"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        name = attrs.location_name,
                        "Failed to create site_parameter"
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

/// Get or create a global parameter record for the given sensor_type name.
/// Returns the UUID of the parameter.
async fn get_or_create_parameter(db: &DatabaseConnection, sensor_type: &str) -> Uuid {
    // Check if it already exists
    if let Ok(Some(existing)) = parameters::Entity::find()
        .filter(parameters::Column::Name.eq(sensor_type))
        .one(db)
        .await
    {
        return existing.id;
    }

    // Derive display name and default units from sensor_type
    let (display_name, default_units) = match sensor_type {
        "Depth" => ("Water Depth", "mm"),
        "CDOM" => ("Colored Dissolved Organic Matter", "ppb"),
        "Turbidity" => ("Turbidity", "NTU"),
        "Battery" => ("Battery Voltage", "V"),
        "DO_Temperature" => ("DO Temperature", "\u{00b0}C"),
        "Dissolved_O2" => ("Dissolved Oxygen", "\u{00b5}M"),
        "Conductivity" => ("Conductivity", "\u{00b5}S/cm"),
        "Cond_Temperature" => ("Conductivity Temperature", "\u{00b0}C"),
        _ => (sensor_type, ""),
    };

    let id = Uuid::new_v4();
    let model = parameters::ActiveModel {
        id: Set(id),
        name: Set(sensor_type.to_string()),
        display_name: Set(display_name.to_string()),
        default_units: Set(default_units.to_string()),
        category: Set("measurement".to_string()),
        data_type: Set("numeric".to_string()),
        description: Set(None),
        created_at: Set(Some(Utc::now())),
    };

    match model.insert(db).await {
        Ok(p) => p.id,
        Err(e) => {
            tracing::warn!(error = %e, sensor_type = sensor_type, "Failed to create parameter, retrying lookup");
            // Race condition: another sync may have created it
            parameters::Entity::find()
                .filter(parameters::Column::Name.eq(sensor_type))
                .one(db)
                .await
                .ok()
                .flatten()
                .map(|p| p.id)
                .unwrap_or(id)
        }
    }
}

/// Create a sensor entity and deployment for a newly discovered Vaisala site_parameter.
/// Returns the sensor UUID if successful.
async fn create_sensor_and_deployment(
    db: &DatabaseConnection,
    serial_number: &str,
    parameter_id: Uuid,
    site_id: Uuid,
    now: chrono::DateTime<Utc>,
) -> Option<Uuid> {
    // Check if sensor with this serial number AND parameter already exists
    let existing = sensors::Entity::find()
        .filter(sensors::Column::SerialNumber.eq(serial_number))
        .filter(sensors::Column::ParameterId.eq(parameter_id))
        .one(db)
        .await
        .ok()
        .flatten();

    let sensor_id = if let Some(sensor) = existing {
        sensor.id
    } else {
        let id = Uuid::new_v4();
        let sensor = sensors::ActiveModel {
            id: Set(id),
            serial_number: Set(Some(serial_number.to_string())),
            name: Set(None),
            parameter_id: Set(parameter_id),
            manufacturer: Set(Some("Vaisala".to_string())),
            model: Set(None),
            is_active: Set(Some(true)),
            is_lab_instrument: Set(Some(false)),
            notes: Set(None),
            created_at: Set(Some(now)),
        };
        match sensor.insert(db).await {
            Ok(s) => {
                // Create identity calibration (slope=1, intercept=0)
                let cal = sensor_calibrations::ActiveModel {
                    id: Set(Uuid::new_v4()),
                    sensor_id: Set(s.id),
                    slope: Set(1.0),
                    intercept: Set(0.0),
                    valid_from: Set(now),
                    performed_by: Set(Some("system".to_string())),
                    notes: Set(Some("Identity calibration (auto-created)".to_string())),
                    created_at: Set(Some(now)),
                };
                if let Err(e) = cal.insert(db).await {
                    tracing::warn!(error = %e, "Failed to create identity calibration");
                }
                s.id
            }
            Err(e) => {
                tracing::warn!(error = %e, serial = serial_number, "Failed to create sensor");
                return None;
            }
        }
    };

    // Create deployment (permanent, starting now)
    let deployment = sensor_deployments::ActiveModel {
        id: Set(Uuid::new_v4()),
        sensor_id: Set(sensor_id),
        site_id: Set(site_id),
        deployed_from: Set(now),
        deployed_until: Set(None),
        deployment_type: Set("permanent".to_string()),
        notes: Set(Some("Auto-created from Vaisala sync".to_string())),
        created_at: Set(Some(now)),
    };

    if let Err(e) = deployment.insert(db).await {
        tracing::warn!(error = %e, "Failed to create sensor deployment");
    }

    Some(sensor_id)
}

/// Derive sensor type from the Vaisala sensor name.
/// E.g., "`MDepthmm`" -> "Depth", "`MCDOMppb`" -> "CDOM"
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
/// E.g., "`MDepthmm`" -> "`WaterDepthmm`", "`MCDOMppb`" -> "`CDOMppb`", "`DDOuM`" -> "`DOuM`"
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
    // Load source mappings for site_parameter source_key -> entity_id
    let mappings: Vec<source_mappings::Model> = source_mappings::Entity::find().all(db).await?;
    let param_map: HashMap<i32, Uuid> = mappings
        .iter()
        .filter(|m| m.entity_type == "site_parameter")
        .map(|m| (m.source_key, m.entity_id))
        .collect();

    let params_with_state: Vec<(site_parameters::Model, Option<sync_state::Model>)> =
        site_parameters::Entity::find()
            .filter(site_parameters::Column::IsActive.eq(true))
            .find_also_related(sync_state::Entity)
            .all(db)
            .await?;

    if params_with_state.is_empty() {
        tracing::debug!("No active site_parameters to sync");
        return Ok(());
    }

    // Build active site_parameter state: entity_id -> last_data_time
    let active_state: HashMap<Uuid, Option<chrono::DateTime<Utc>>> = params_with_state
        .iter()
        .map(|(p, state)| {
            let last_time = if force_full_sync {
                None
            } else {
                state
                    .as_ref()
                    .and_then(|s| s.last_data_time.map(|dt| dt.with_timezone(&Utc)))
            };
            (p.id, last_time)
        })
        .collect();

    // Build location_map: source_key -> (entity_id, last_data_time) for active mapped parameters
    let mut location_map: HashMap<i32, (Uuid, Option<chrono::DateTime<Utc>>)> = HashMap::new();
    for (source_key, entity_id) in &param_map {
        if let Some(last_time) = active_state.get(entity_id) {
            location_map.insert(*source_key, (*entity_id, *last_time));
        }
    }

    if location_map.is_empty() {
        tracing::debug!("No mapped active site_parameters to sync");
        return Ok(());
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
            for (sp, _) in &params_with_state {
                update_sync_state_error(db, sp.id, &e.to_string()).await;
            }
            return Err(e);
        }
    };

    // Build site_parameter_id -> (site_id, global parameter_id) map for readings and derived computation
    let sp_info_map: HashMap<Uuid, (Uuid, Uuid)> = params_with_state
        .iter()
        .map(|(sp, _)| (sp.id, (sp.site_id, sp.parameter_id)))
        .collect();

    // Build sensor/calibration/deployment lookup for each site_parameter.
    // This enables recalculate_for_calibration() to find readings by sensor_id.
    let sensor_lookup: HashMap<Uuid, (Option<Uuid>, Option<Uuid>, Option<Uuid>)> = {
        let mut lookup = HashMap::new();
        for (sp_id, (site_id, param_id)) in &sp_info_map {
            // Find active deployment for this site_parameter's parameter at this site.
            // Join through sensors to match the global parameter_id.
            let deployment = sensor_deployments::Entity::find()
                .filter(sensor_deployments::Column::SiteId.eq(*site_id))
                .filter(sensor_deployments::Column::DeployedUntil.is_null())
                .find_also_related(sensors::Entity)
                .all(db)
                .await
                .unwrap_or_default()
                .into_iter()
                .find(|(_dep, sensor)| {
                    sensor.as_ref().is_some_and(|s| s.parameter_id == *param_id)
                });

            if let Some((dep, _sensor)) = deployment {
                // Find the latest calibration for this sensor (valid_from <= now)
                let calibration = sensor_calibrations::Entity::find()
                    .filter(sensor_calibrations::Column::SensorId.eq(dep.sensor_id))
                    .order_by_desc(sensor_calibrations::Column::ValidFrom)
                    .one(db)
                    .await
                    .ok()
                    .flatten();

                lookup.insert(
                    *sp_id,
                    (Some(dep.sensor_id), calibration.map(|c| c.id), Some(dep.id)),
                );
            } else {
                lookup.insert(*sp_id, (None, None, None));
            }
        }
        lookup
    };

    // Track affected timestamps per site for derived parameter computation
    let mut site_timestamps: HashMap<Uuid, HashSet<chrono::DateTime<Utc>>> = HashMap::new();

    for resource in history.data {
        let attrs = resource.attributes;
        let Some((site_parameter_id, last_time)) = location_map.get(&attrs.id) else {
            tracing::warn!(location_id = attrs.id, "Received data for unknown location");
            continue;
        };

        let Some((site_id, parameter_id)) = sp_info_map.get(site_parameter_id) else {
            tracing::warn!(
                site_parameter_id = %site_parameter_id,
                "Could not resolve site_id/parameter_id for site_parameter"
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
                site_parameter_id = %site_parameter_id,
                location_id = attrs.id,
                "No new samples"
            );
            continue;
        }

        let sample_count = new_points.len();

        let mut models: Vec<readings::ActiveModel> = Vec::with_capacity(new_points.len());
        let mut latest_timestamp: Option<i64> = None;
        let mut rounded_times: Vec<chrono::DateTime<Utc>> = Vec::with_capacity(new_points.len());

        for point in new_points {
            let raw_time =
                chrono::DateTime::from_timestamp(point.timestamp, 0).unwrap_or_else(Utc::now);
            let epoch = raw_time.timestamp();
            let rounded_epoch = ((epoch + 300) / 600) * 600;
            let time = chrono::DateTime::from_timestamp(rounded_epoch, 0).unwrap_or(raw_time);

            rounded_times.push(time);

            let (s_id, c_id, d_id) = sensor_lookup
                .get(site_parameter_id)
                .copied()
                .unwrap_or((None, None, None));

            models.push(readings::ActiveModel {
                site_id: Set(*site_id),
                parameter_id: Set(*parameter_id),
                time: Set(time.into()),
                raw_value: Set(point.value),
                calibrated_value: Set(Some(point.value)), // Identity calibration: calibrated = raw
                sensor_id: Set(s_id),
                calibration_id: Set(c_id),
                deployment_id: Set(d_id),
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
                        readings::Column::SiteId,
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
            update_sync_state_success(db, *site_parameter_id, latest).await;
        }

        tracing::info!(
            count = sample_count,
            site_parameter_id = %site_parameter_id,
            location_id = attrs.id,
            "Synced readings"
        );

        // Track rounded timestamps per site for derived computation
        let entry = site_timestamps.entry(*site_id).or_default();
        for t in rounded_times {
            entry.insert(t);
        }
    }

    // Compute derived parameter values for affected timestamps
    let mut derived_count = 0u64;
    let mut derived_sites = 0u64;
    for (site_id, timestamps) in &site_timestamps {
        derived_sites += 1;
        for time in timestamps {
            match recalculate_derived_at_timestamp(db, *site_id, *time).await {
                Ok(()) => derived_count += 1,
                Err(e) => tracing::warn!(
                    error = %e, site_id = %site_id, time = %time,
                    "Failed to compute derived values"
                ),
            }
        }
    }
    if derived_count > 0 {
        tracing::info!(
            derived_timestamps = derived_count,
            sites = derived_sites,
            "Computed derived parameter values"
        );
    }

    Ok(())
}

/// Create an alarm threshold record based on sensor type.
fn create_threshold_for_sensor_type(
    parameter_id: Uuid,
    site_id: Uuid,
    sensor_type: &str,
) -> alarm_thresholds::ActiveModel {
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

    let now = Utc::now();
    alarm_thresholds::ActiveModel {
        id: Set(Uuid::new_v4()),
        parameter_id: Set(parameter_id),
        site_id: Set(Some(site_id)),
        warning_min: Set(warning_min),
        warning_max: Set(warning_max),
        alarm_min: Set(alarm_min),
        alarm_max: Set(alarm_max),
        description: Set(Some("Auto-generated from sensor type defaults".to_string())),
        string_alarm_values: Set(None),
        string_warning_values: Set(None),
        created_at: Set(Some(now)),
        updated_at: Set(Some(now)),
    }
}
