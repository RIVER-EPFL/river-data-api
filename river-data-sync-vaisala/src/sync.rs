use chrono::{Duration, Utc};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use crate::api_client::ApiClient;
use crate::models::{ReadingInput, StatusEventInput};
use crate::vaisala_client::{SyncError, VaisalaClient};

const BATCH_SIZE: usize = 1000;

/// Discover and sync projects, sites, and parameters from Vaisala via the API.
pub async fn sync_locations(
    api: &ApiClient,
    vaisala: &VaisalaClient,
) -> Result<(), SyncError> {
    tracing::info!("Discovering locations from Vaisala...");

    let locations = vaisala.get_locations().await?;

    // Load all source mappings for dedup and rename detection
    let mappings = api.list_source_mappings(None).await?;

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

    // Build project name -> UUID map
    let existing_projects = api.list_projects().await?;
    let mut project_ids: HashMap<String, Uuid> = existing_projects
        .into_iter()
        .map(|p| (p.name.clone(), p.id))
        .collect();

    // Build site source_key -> UUID map
    let mut site_ids: HashMap<i32, Uuid> = site_mappings
        .iter()
        .map(|(key, (id, _))| (*key, *id))
        .collect();

    let mut projects_created = 0u32;
    let mut sites_created = 0u32;
    let mut params_created = 0u32;
    let mut new_param_location_ids: Vec<i32> = Vec::new();

    // Load existing parameters for get_or_create
    let existing_params = api.list_parameters().await?;
    let mut param_name_to_id: HashMap<String, Uuid> = existing_params
        .into_iter()
        .map(|p| (p.name.clone(), p.id))
        .collect();

    for resource in &locations.data {
        let attrs = &resource.attributes;
        if attrs.deleted {
            continue;
        }

        let parts: Vec<&str> = attrs.path.split('/').collect();

        match (parts.len(), attrs.leaf) {
            // Project
            (2, false) => {
                let project_name = parts[1];

                if let Some((_, old_name)) = project_mappings.get(&attrs.node_id) {
                    if let Some(old) = old_name {
                        if old != project_name {
                            tracing::info!(
                                source_key = attrs.node_id,
                                old_name = old,
                                new_name = project_name,
                                "Vaisala project renamed"
                            );
                            let _ = api
                                .upsert_source_mapping(&serde_json::json!({
                                    "entity_type": "project",
                                    "source_key": attrs.node_id,
                                    "entity_id": project_mappings[&attrs.node_id].0,
                                    "source_name": project_name,
                                    "source_system": "vaisala",
                                }))
                                .await;
                        }
                    }
                    continue;
                }

                // Create new project
                match api
                    .create_project(&serde_json::json!({
                        "name": project_name,
                        "data_source": "vaisala",
                        "description": if attrs.description.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(attrs.description.clone()) },
                    }))
                    .await
                {
                    Ok(p) => {
                        let _ = api
                            .upsert_source_mapping(&serde_json::json!({
                                "entity_type": "project",
                                "source_key": attrs.node_id,
                                "entity_id": p.id,
                                "source_name": project_name,
                                "source_system": "vaisala",
                            }))
                            .await;
                        project_ids.insert(project_name.to_string(), p.id);
                        projects_created += 1;
                    }
                    Err(e) => tracing::warn!(error = %e, name = project_name, "Failed to create project"),
                }
            }

            // Site
            (3, false) => {
                let project_name = parts[1];
                let site_name = parts[2];

                if let Some((_, old_name)) = site_mappings.get(&attrs.node_id) {
                    if let Some(old) = old_name {
                        if old != site_name {
                            tracing::info!(
                                source_key = attrs.node_id,
                                old_name = old,
                                new_name = site_name,
                                "Vaisala site renamed"
                            );
                            let _ = api
                                .upsert_source_mapping(&serde_json::json!({
                                    "entity_type": "site",
                                    "source_key": attrs.node_id,
                                    "entity_id": site_mappings[&attrs.node_id].0,
                                    "source_name": site_name,
                                    "source_system": "vaisala",
                                }))
                                .await;
                        }
                    }
                    continue;
                }

                let project_id = project_ids.get(project_name).copied();
                match api
                    .create_site(&serde_json::json!({
                        "name": site_name,
                        "project_id": project_id,
                    }))
                    .await
                {
                    Ok(s) => {
                        let _ = api
                            .upsert_source_mapping(&serde_json::json!({
                                "entity_type": "site",
                                "source_key": attrs.node_id,
                                "entity_id": s.id,
                                "source_name": site_name,
                                "source_system": "vaisala",
                            }))
                            .await;
                        site_ids.insert(attrs.node_id, s.id);
                        sites_created += 1;
                    }
                    Err(e) => tracing::warn!(error = %e, name = site_name, "Failed to create site"),
                }
            }

            // Parameter (leaf)
            (_, true) if parts.len() >= 4 => {
                if !param_mappings.contains_key(&attrs.node_id) {
                    new_param_location_ids.push(attrs.node_id);
                }
            }

            _ => {}
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
                tracing::warn!(location_id = attrs.id, path = attrs.location_path, "Could not find site");
                continue;
            };

            let sensor_type = derive_parameter_type(&attrs.location_name);
            let generic_name = derive_generic_name(&attrs.location_name);

            // Get or create global parameter
            let global_param_id = if let Some(id) = param_name_to_id.get(&sensor_type) {
                *id
            } else {
                let (display_name, default_units) = parameter_defaults(&sensor_type);
                match api
                    .create_parameter(&serde_json::json!({
                        "name": sensor_type,
                        "display_name": display_name,
                        "default_units": default_units,
                        "category": derive_category(&sensor_type),
                        "data_type": "numeric",
                    }))
                    .await
                {
                    Ok(p) => {
                        param_name_to_id.insert(sensor_type.clone(), p.id);
                        p.id
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, sensor_type = sensor_type, "Failed to create parameter");
                        continue;
                    }
                }
            };

            // Create site_parameter
            match api
                .create_site_parameter(&serde_json::json!({
                    "site_id": site_id,
                    "parameter_id": global_param_id,
                    "name": generic_name,
                    "sensor_type": sensor_type,
                    "display_units": attrs.display_units,
                    "decimal_places": attrs.decimal_places,
                    "channel_id": if attrs.channel_id == 0 { None } else { Some(attrs.channel_id) },
                    "sample_interval_sec": if attrs.sample_interval_sec == 0 { None } else { Some(attrs.sample_interval_sec) },
                    "is_active": true,
                    "is_derived": false,
                }))
                .await
            {
                Ok(sp) => {
                    // Create source mapping
                    let _ = api
                        .upsert_source_mapping(&serde_json::json!({
                            "entity_type": "site_parameter",
                            "source_key": attrs.id,
                            "entity_id": sp.id,
                            "source_name": attrs.location_name,
                            "source_system": "vaisala",
                        }))
                        .await;

                    // Initialize sync state
                    let _ = api
                        .create_sync_state(&serde_json::json!({
                            "site_parameter_id": sp.id,
                            "sync_status": "pending",
                            "retry_count": 0,
                        }))
                        .await;

                    // Create alarm threshold based on sensor_type
                    let (warning_min, warning_max, alarm_min, alarm_max) =
                        threshold_defaults(&sensor_type);
                    if let Err(e) = api
                        .create_alarm_threshold(&serde_json::json!({
                            "parameter_id": global_param_id,
                            "site_id": site_id,
                            "warning_min": warning_min,
                            "warning_max": warning_max,
                            "alarm_min": alarm_min,
                            "alarm_max": alarm_max,
                            "description": "Auto-generated from sensor type defaults",
                        }))
                        .await
                    {
                        tracing::warn!(error = %e, "Failed to create alarm threshold");
                    }

                    // Create sensor + identity calibration + deployment
                    if !attrs.logger_serial_number.is_empty() {
                        create_sensor_and_deployment_via_api(
                            api,
                            &attrs.logger_serial_number,
                            global_param_id,
                            site_id,
                        )
                        .await;
                    }

                    params_created += 1;
                    tracing::debug!(name = generic_name, location_id = attrs.id, "Created site_parameter");
                }
                Err(e) => tracing::warn!(error = %e, name = attrs.location_name, "Failed to create site_parameter"),
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

/// Sync readings for all active parameters via the API.
pub async fn sync_readings(
    api: &ApiClient,
    vaisala: &VaisalaClient,
    max_history_days: i64,
    force_full_sync: bool,
) -> Result<(), SyncError> {
    // Load source mappings for site_parameter
    let mappings = api.list_source_mappings(Some("site_parameter")).await?;
    let param_map: HashMap<i32, Uuid> = mappings
        .iter()
        .map(|m| (m.source_key, m.entity_id))
        .collect();

    // Load site_parameters and sync states
    let site_params = api.list_site_parameters().await?;
    let sync_states = api.list_sync_states().await?;
    let sync_state_map: HashMap<Uuid, crate::models::SyncState> = sync_states
        .into_iter()
        .map(|s| (s.site_parameter_id, s))
        .collect();

    let active_params: Vec<_> = site_params
        .iter()
        .filter(|sp| sp.is_active.unwrap_or(true) && !sp.is_derived.unwrap_or(false))
        .collect();

    if active_params.is_empty() {
        tracing::debug!("No active site_parameters to sync");
        return Ok(());
    }

    // Build location_map: source_key -> (site_parameter_id, site_id, parameter_id, last_data_time)
    let now = Utc::now();
    let max_history_start = now - Duration::days(max_history_days);

    let mut location_map: HashMap<i32, (Uuid, Uuid, Uuid, Option<chrono::DateTime<Utc>>)> =
        HashMap::new();

    for (source_key, sp_id) in &param_map {
        if let Some(sp) = active_params.iter().find(|p| p.id == *sp_id) {
            let last_time = if force_full_sync {
                None
            } else {
                sync_state_map
                    .get(sp_id)
                    .and_then(|s| s.last_data_time)
            };
            location_map.insert(*source_key, (*sp_id, sp.site_id, sp.parameter_id, last_time));
        }
    }

    if location_map.is_empty() {
        tracing::debug!("No mapped active site_parameters to sync");
        return Ok(());
    }

    let location_ids: Vec<i32> = location_map.keys().copied().collect();
    let earliest_from = location_map
        .values()
        .map(|(_, _, _, last_time)| last_time.unwrap_or(max_history_start))
        .min()
        .unwrap_or(max_history_start);

    tracing::info!(
        parameter_count = location_ids.len(),
        from = %earliest_from,
        "Syncing readings"
    );

    let history = vaisala
        .get_locations_history(&location_ids, earliest_from, Some(now))
        .await?;

    // Track affected timestamps per site for derived computation
    let mut site_timestamps: HashMap<Uuid, HashSet<chrono::DateTime<Utc>>> = HashMap::new();

    for resource in history.data {
        let attrs = resource.attributes;
        let Some((sp_id, site_id, parameter_id, last_time)) = location_map.get(&attrs.id) else {
            continue;
        };

        let last_timestamp = last_time.map(|lt| lt.timestamp());
        let new_points: Vec<_> = attrs
            .data_points
            .into_iter()
            .filter(|dp| last_timestamp.is_none_or(|lt| dp.timestamp > lt))
            .collect();

        if new_points.is_empty() {
            continue;
        }

        let sample_count = new_points.len();
        let mut readings: Vec<ReadingInput> = Vec::with_capacity(new_points.len());
        let mut latest_timestamp: Option<i64> = None;
        let mut rounded_times: Vec<chrono::DateTime<Utc>> = Vec::with_capacity(new_points.len());

        for point in new_points {
            let raw_time =
                chrono::DateTime::from_timestamp(point.timestamp, 0).unwrap_or_else(Utc::now);
            let epoch = raw_time.timestamp();
            let rounded_epoch = ((epoch + 300) / 600) * 600;
            let time = chrono::DateTime::from_timestamp(rounded_epoch, 0).unwrap_or(raw_time);

            rounded_times.push(time);

            readings.push(ReadingInput {
                site_id: *site_id,
                parameter_id: *parameter_id,
                time,
                raw_value: point.value,
                calibrated_value: Some(point.value),
                sensor_id: None,
                calibration_id: None,
                deployment_id: None,
            });

            if latest_timestamp.is_none_or(|lt| point.timestamp > lt) {
                latest_timestamp = Some(point.timestamp);
            }
        }

        // Insert in batches
        for chunk in readings.chunks(BATCH_SIZE) {
            if let Err(e) = api.insert_readings_batch(chunk).await {
                tracing::warn!(error = %e, batch_size = chunk.len(), "Failed to insert reading batch");
            }
        }

        // Update sync state
        if let Some(ts) = latest_timestamp {
            if let Some(latest) = chrono::DateTime::from_timestamp(ts, 0) {
                let _ = api
                    .update_sync_state(
                        *sp_id,
                        &serde_json::json!({
                            "last_data_time": latest.to_rfc3339(),
                            "last_sync_attempt": Utc::now().to_rfc3339(),
                            "sync_status": "success",
                            "error_message": null,
                            "retry_count": 0,
                        }),
                    )
                    .await;
            }
        }

        tracing::info!(
            count = sample_count,
            site_parameter_id = %sp_id,
            location_id = attrs.id,
            "Synced readings"
        );

        // Track timestamps for derived computation
        let entry = site_timestamps.entry(*site_id).or_default();
        for t in rounded_times {
            entry.insert(t);
        }
    }

    // Compute derived parameter values
    if !site_timestamps.is_empty() {
        let entries: Vec<(Uuid, Vec<chrono::DateTime<Utc>>)> = site_timestamps
            .into_iter()
            .map(|(site_id, timestamps)| (site_id, timestamps.into_iter().collect()))
            .collect();

        if let Err(e) = api.compute_derived(&entries).await {
            tracing::warn!(error = %e, "Failed to trigger derived computation");
        }
    }

    Ok(())
}

/// Ensure a device_health global parameter exists, returning its UUID.
///
/// Looks up `name` in `param_cache`; if missing, creates it via the API and
/// inserts the resulting UUID into the cache.
async fn ensure_health_parameter(
    api: &ApiClient,
    param_cache: &mut HashMap<String, Uuid>,
    name: &str,
    display_name: &str,
    data_type: &str,
) -> Result<Uuid, SyncError> {
    if let Some(id) = param_cache.get(name) {
        return Ok(*id);
    }
    let p = api
        .create_parameter(&serde_json::json!({
            "name": name,
            "display_name": display_name,
            "default_units": "",
            "category": "device_health",
            "data_type": data_type,
        }))
        .await?;
    param_cache.insert(name.to_string(), p.id);
    Ok(p.id)
}

/// Sync device status from Vaisala into status_events via the API.
///
/// Calls `get_locations_data` for all mapped active parameters and posts
/// device_status, battery_level, signal_quality, line_powered, and unreachable
/// values to the status_events batch endpoint.
pub async fn sync_device_status(
    api: &ApiClient,
    vaisala: &VaisalaClient,
) -> Result<(), SyncError> {
    let mappings = api.list_source_mappings(Some("site_parameter")).await?;
    let param_map: HashMap<i32, Uuid> = mappings
        .iter()
        .map(|m| (m.source_key, m.entity_id))
        .collect();

    let site_params = api.list_site_parameters().await?;
    let active_params: Vec<_> = site_params
        .iter()
        .filter(|sp| sp.is_active.unwrap_or(true))
        .collect();

    // Build sp_id -> (site_id, parameter_id) lookup
    let sp_info: HashMap<Uuid, (Uuid, Uuid)> = active_params
        .iter()
        .map(|sp| (sp.id, (sp.site_id, sp.parameter_id)))
        .collect();

    let location_ids: Vec<i32> = param_map
        .iter()
        .filter(|(_, sp_id)| sp_info.contains_key(sp_id))
        .map(|(key, _)| *key)
        .collect();

    if location_ids.is_empty() {
        tracing::debug!("No mapped parameters for device status sync");
        return Ok(());
    }

    tracing::info!(
        location_count = location_ids.len(),
        "Syncing device status"
    );

    let data = vaisala.get_locations_data(&location_ids).await?;
    let now = Utc::now();

    // Build a cache of existing global parameters by name
    let existing_params = api.list_parameters().await?;
    let mut param_cache: HashMap<String, Uuid> = existing_params
        .into_iter()
        .map(|p| (p.name.clone(), p.id))
        .collect();

    // Ensure all device_health parameters exist
    let device_status_id = ensure_health_parameter(
        api, &mut param_cache, "Device_Status", "Device Status", "string",
    ).await?;
    let battery_level_id = ensure_health_parameter(
        api, &mut param_cache, "Battery_Level", "Battery Level", "integer",
    ).await?;
    let signal_quality_id = ensure_health_parameter(
        api, &mut param_cache, "Signal_Quality", "Signal Quality", "integer",
    ).await?;
    let line_powered_id = ensure_health_parameter(
        api, &mut param_cache, "Line_Powered", "Line Powered", "integer",
    ).await?;
    let unreachable_id = ensure_health_parameter(
        api, &mut param_cache, "Unreachable", "Unreachable", "boolean",
    ).await?;

    // Collect one set of health events per site per sync cycle
    let mut seen_sites: HashSet<Uuid> = HashSet::new();
    let mut events: Vec<StatusEventInput> = Vec::new();

    for resource in data.data {
        let attrs = resource.attributes;

        let Some(sp_id) = param_map.get(&attrs.id) else {
            continue;
        };
        let Some((site_id, _)) = sp_info.get(sp_id) else {
            continue;
        };

        // One set of health events per site per sync cycle
        if !seen_sites.insert(*site_id) {
            continue;
        }

        // Device status (string) — only if non-empty
        if !attrs.device_status.is_empty() {
            events.push(StatusEventInput {
                site_id: *site_id,
                parameter_id: device_status_id,
                time: now,
                value: attrs.device_status.clone(),
                sensor_id: None,
            });
        }

        // Battery level (integer)
        events.push(StatusEventInput {
            site_id: *site_id,
            parameter_id: battery_level_id,
            time: now,
            value: attrs.battery_level.to_string(),
            sensor_id: None,
        });

        // Signal quality (integer)
        events.push(StatusEventInput {
            site_id: *site_id,
            parameter_id: signal_quality_id,
            time: now,
            value: attrs.signal_quality.to_string(),
            sensor_id: None,
        });

        // Line powered (integer)
        events.push(StatusEventInput {
            site_id: *site_id,
            parameter_id: line_powered_id,
            time: now,
            value: attrs.line_powered.to_string(),
            sensor_id: None,
        });

        // Unreachable (boolean)
        events.push(StatusEventInput {
            site_id: *site_id,
            parameter_id: unreachable_id,
            time: now,
            value: attrs.unreachable.to_string(),
            sensor_id: None,
        });
    }

    if events.is_empty() {
        tracing::debug!("No device status events to insert");
        return Ok(());
    }

    match api.insert_status_events_batch(&events).await {
        Ok(count) => tracing::info!(inserted = count, "Device status sync complete"),
        Err(e) => tracing::warn!(error = %e, "Failed to insert status events"),
    }

    Ok(())
}

/// Derive the parameter category from the sensor type.
fn derive_category(sensor_type: &str) -> &'static str {
    match sensor_type {
        "Battery" => "device_health",
        _ => "measurement",
    }
}

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

    name.to_string()
}

fn derive_generic_name(vaisala_name: &str) -> String {
    let mappings: &[(&str, &str)] = &[
        ("Depthmm", "WaterDepthmm"),
        ("CDOMppb", "CDOMppb"),
        ("TurbNTU", "TurbiNTU"),
        ("BattV", "BattV"),
        ("DOdegC", "WaterTempdegC"),
        ("DOTdegC", "WaterTempdegC"),
        ("DOuM", "DOuM"),
        ("ConduSCm", "ConduScm"),
        ("ConduScm", "ConduScm"),
        ("CondTdegC", "CondTempdegC"),
    ];

    if vaisala_name.len() > 1 {
        let without_prefix = &vaisala_name[1..];
        for (suffix, generic) in mappings {
            if without_prefix == *suffix || without_prefix.ends_with(suffix) {
                return (*generic).to_string();
            }
        }
    }

    vaisala_name.to_string()
}

fn threshold_defaults(
    sensor_type: &str,
) -> (Option<f64>, Option<f64>, Option<f64>, Option<f64>) {
    match sensor_type {
        "Depth" => (Some(100.0), Some(1000.0), Some(0.0), Some(2000.0)),
        "CDOM" => (None, Some(100.0), Some(0.0), Some(150.0)),
        "Turbidity" => (None, Some(100.0), Some(0.0), Some(500.0)),
        "Dissolved_O2" => (Some(120.0), Some(360.0), Some(0.0), Some(625.0)),
        "Conductivity" => (Some(100.0), Some(900.0), Some(0.0), Some(1000.0)),
        "DO_Temperature" | "Cond_Temperature" => (Some(0.5), Some(20.0), Some(0.0), Some(25.0)),
        "Battery" => (Some(12.1), None, Some(11.5), None),
        _ => (None, None, None, None),
    }
}

async fn create_sensor_and_deployment_via_api(
    api: &ApiClient,
    serial_number: &str,
    parameter_id: Uuid,
    site_id: Uuid,
) {
    let now = chrono::Utc::now().to_rfc3339();

    // Create sensor
    let sensor_result = api
        .create_sensor(&serde_json::json!({
            "serial_number": serial_number,
            "parameter_id": parameter_id,
            "manufacturer": "Vaisala",
            "is_active": true,
            "is_lab_instrument": false,
        }))
        .await;

    let sensor_id = match sensor_result {
        Ok(s) => match s.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                tracing::warn!("Sensor created but no id returned");
                return;
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, serial = serial_number, "Failed to create sensor");
            return;
        }
    };

    // Create identity calibration (slope=1, intercept=0)
    if let Err(e) = api
        .create_sensor_calibration(&serde_json::json!({
            "sensor_id": sensor_id,
            "slope": 1.0,
            "intercept": 0.0,
            "valid_from": now,
            "performed_by": "system",
            "notes": "Identity calibration (auto-created)",
        }))
        .await
    {
        tracing::warn!(error = %e, "Failed to create identity calibration");
    }

    // Create deployment (permanent, starting now)
    if let Err(e) = api
        .create_sensor_deployment(&serde_json::json!({
            "sensor_id": sensor_id,
            "site_id": site_id,
            "deployed_from": now,
            "deployment_type": "permanent",
            "notes": "Auto-created from Vaisala sync",
        }))
        .await
    {
        tracing::warn!(error = %e, "Failed to create sensor deployment");
    }
}

fn parameter_defaults(sensor_type: &str) -> (&str, &str) {
    match sensor_type {
        "Depth" => ("Water Depth", "mm"),
        "CDOM" => ("Colored Dissolved Organic Matter", "ppb"),
        "Turbidity" => ("Turbidity", "NTU"),
        "Battery" => ("Battery Voltage", "V"),
        "DO_Temperature" => ("DO Temperature", "\u{00b0}C"),
        "Dissolved_O2" => ("Dissolved Oxygen", "\u{00b5}M"),
        "Conductivity" => ("Conductivity", "\u{00b5}S/cm"),
        "Cond_Temperature" => ("Conductivity Temperature", "\u{00b0}C"),
        _ => (sensor_type, ""),
    }
}
