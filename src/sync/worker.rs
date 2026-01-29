use chrono::{Duration, Utc};
use sea_orm::{ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set, Statement};
use std::collections::HashMap;
use uuid::Uuid;

use crate::entity::{
    alarm_locations, alarms, device_status, events, readings, sensors, stations, sync_state, zones,
};
use crate::error::AppResult;
use crate::vaisala::VaisalaClient;

/// Batch size for bulk inserts
const BATCH_SIZE: usize = 1000;

/// Discover and sync zones, stations, and sensors from Vaisala.
///
/// Parses the location hierarchy from Vaisala's `/locations` endpoint and creates
/// any missing zones, stations, or sensors in the database.
///
/// Hierarchy (based on path depth):
/// - viewLinc (root, ignored)
///   - Zone (depth 1, e.g., "BREATHE")
///     - Station (depth 2, e.g., "Martigny")
///       - Sensor (depth 3, leaf=true, e.g., "MDepthmm")
///
/// # Errors
///
/// Returns an error if the Vaisala API or database operations fail.
pub async fn sync_locations(db: &DatabaseConnection, vaisala: &VaisalaClient) -> AppResult<()> {
    tracing::info!("Discovering locations from Vaisala...");

    // Fetch all locations from Vaisala
    let locations = vaisala.get_locations().await?;

    let now = Utc::now();

    // Build maps of existing entities by their Vaisala identifiers
    let existing_zones: HashMap<String, zones::Model> = zones::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|z| (z.name.clone(), z))
        .collect();

    let existing_stations: HashMap<i32, stations::Model> = stations::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.vaisala_node_id, s))
        .collect();

    let existing_sensors: HashMap<i32, sensors::Model> = sensors::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.vaisala_location_id, s))
        .collect();

    // Track created entities for logging
    let mut zones_created = 0;
    let mut stations_created = 0;
    let mut sensors_created = 0;

    // Maps to track newly created zones/stations by name for FK lookups
    let mut zone_ids: HashMap<String, Uuid> = existing_zones
        .iter()
        .map(|(name, z)| (name.clone(), z.id))
        .collect();
    let mut station_ids: HashMap<i32, Uuid> = existing_stations
        .iter()
        .map(|(node_id, s)| (*node_id, s.id))
        .collect();

    // Collect sensor location IDs for fetching detailed info
    let mut new_sensor_location_ids: Vec<i32> = Vec::new();

    // Process each location
    for resource in &locations.data {
        let attrs = &resource.attributes;

        // Skip deleted locations
        if attrs.deleted {
            continue;
        }

        // Parse path segments: "viewLinc/BREATHE/Martigny/MDepthmm"
        let parts: Vec<&str> = attrs.path.split('/').collect();

        // Determine entity type based on path depth and leaf status
        // parts[0] = "viewLinc" (root, skip)
        // parts[1] = Zone name (depth 1)
        // parts[2] = Station name (depth 2)
        // parts[3+] = Sensor (leaf=true)

        match (parts.len(), attrs.leaf) {
            // Zone: path like "viewLinc/BREATHE" (2 parts, not leaf)
            (2, false) => {
                let zone_name = parts[1];
                if !zone_ids.contains_key(zone_name) {
                    let zone = zones::ActiveModel {
                        id: Set(Uuid::new_v4()),
                        name: Set(zone_name.to_string()),
                        vaisala_path: Set(Some(attrs.path.clone())),
                        description: Set(if attrs.description.is_empty() {
                            None
                        } else {
                            Some(attrs.description.clone())
                        }),
                        created_at: Set(Some(now.into())),
                        discovered_at: Set(Some(now.into())),
                    };

                    match zone.insert(db).await {
                        Ok(z) => {
                            zone_ids.insert(zone_name.to_string(), z.id);
                            zones_created += 1;
                            tracing::debug!(name = zone_name, "Created zone");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, name = zone_name, "Failed to create zone");
                        }
                    }
                }
            }

            // Station: path like "viewLinc/BREATHE/Martigny" (3 parts, not leaf)
            (3, false) => {
                let zone_name = parts[1];
                let station_name = parts[2];

                if !station_ids.contains_key(&attrs.node_id) {
                    let zone_id = zone_ids.get(zone_name).copied();

                    let station = stations::ActiveModel {
                        id: Set(Uuid::new_v4()),
                        zone_id: Set(zone_id),
                        name: Set(station_name.to_string()),
                        vaisala_node_id: Set(attrs.node_id),
                        vaisala_path: Set(Some(attrs.path.clone())),
                        latitude: Set(None),
                        longitude: Set(None),
                        altitude_m: Set(None),
                        created_at: Set(Some(now.into())),
                        discovered_at: Set(Some(now.into())),
                    };

                    match station.insert(db).await {
                        Ok(s) => {
                            station_ids.insert(attrs.node_id, s.id);
                            stations_created += 1;
                            tracing::debug!(name = station_name, node_id = attrs.node_id, "Created station");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, name = station_name, "Failed to create station");
                        }
                    }
                }
            }

            // Sensor: leaf=true with path like "viewLinc/BREATHE/Martigny/MDepthmm"
            (_, true) if parts.len() >= 4 => {
                if !existing_sensors.contains_key(&attrs.node_id) {
                    new_sensor_location_ids.push(attrs.node_id);
                }
            }

            _ => {
                // Other hierarchy depths or patterns - skip
            }
        }
    }

    // Fetch detailed info for new sensors and create them
    if !new_sensor_location_ids.is_empty() {
        tracing::debug!(count = new_sensor_location_ids.len(), "Fetching sensor details");

        let sensor_data = vaisala.get_locations_data(&new_sensor_location_ids).await?;

        for resource in sensor_data.data {
            let attrs = resource.attributes;

            // Parse path to get station node_id
            let parts: Vec<&str> = attrs.location_path.split('/').collect();
            if parts.len() < 4 {
                continue;
            }

            // Find station by looking up in our locations data
            // The station path would be parts[0..3].join("/")
            let station_path = parts[..3].join("/");

            // Find the station's node_id from our locations response
            let station_node_id = locations
                .data
                .iter()
                .find(|r| r.attributes.path == station_path)
                .map(|r| r.attributes.node_id);

            let Some(station_id) = station_node_id.and_then(|nid| station_ids.get(&nid).copied()) else {
                tracing::warn!(
                    location_id = attrs.id,
                    path = attrs.location_path,
                    "Could not find station for sensor"
                );
                continue;
            };

            // Derive sensor_type from the name (e.g., "MDepthmm" -> "Depth")
            // This is a simple heuristic; adjust as needed
            let sensor_type = derive_sensor_type(&attrs.location_name);

            let sensor = sensors::ActiveModel {
                id: Set(Uuid::new_v4()),
                station_id: Set(station_id),
                vaisala_location_id: Set(attrs.id),
                name: Set(attrs.location_name.clone()),
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

            match sensor.insert(db).await {
                Ok(s) => {
                    // Initialize sync_state for the new sensor
                    let sync = sync_state::ActiveModel {
                        sensor_id: Set(s.id),
                        last_data_time: Set(None),
                        last_sync_attempt: Set(None),
                        sync_status: Set(Some("pending".to_string())),
                        error_message: Set(None),
                        retry_count: Set(Some(0)),
                        last_full_sync: Set(None),
                    };
                    let _ = sync.insert(db).await;

                    sensors_created += 1;
                    tracing::debug!(
                        name = attrs.location_name,
                        location_id = attrs.id,
                        "Created sensor"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        name = attrs.location_name,
                        "Failed to create sensor"
                    );
                }
            }
        }
    }

    tracing::info!(
        zones = zones_created,
        stations = stations_created,
        sensors = sensors_created,
        "Location discovery complete"
    );

    Ok(())
}

/// Derive sensor type from the sensor name.
/// E.g., "MDepthmm" -> "Depth", "MCDOMppb" -> "CDOM"
fn derive_sensor_type(name: &str) -> String {
    // Common patterns: first char is station prefix, then type, then units
    // MDepthmm, MCDOMppb, MTurbNTU, MBattV, MDOdegC, MConduSCm, MDOuM, MCondTdegC
    let patterns: &[(&str, &[&str])] = &[
        ("Depth", &["depth", "Depth"]),
        ("CDOM", &["cdom", "CDOM"]),
        ("Turbidity", &["turb", "Turb"]),
        ("Battery", &["batt", "Batt"]),
        ("DO_Temperature", &["DOdegC", "DOTdegC"]),
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

/// Sync readings for all active sensors.
///
/// If `force_full_sync` is true, ignores `last_data_time` and fetches the full
/// history (up to `max_history_days`). This is used for periodic full re-syncs
/// to catch any backfilled data from Vaisala.
///
/// # Errors
///
/// Returns an error if the database or Vaisala API operations fail.
pub async fn sync_readings(
    db: &DatabaseConnection,
    vaisala: &VaisalaClient,
    max_history_days: i64,
    force_full_sync: bool,
) -> AppResult<()> {
    // Get all active sensors with their sync state
    let sensors_with_state: Vec<(sensors::Model, Option<sync_state::Model>)> =
        sensors::Entity::find()
            .filter(sensors::Column::IsActive.eq(true))
            .find_also_related(sync_state::Entity)
            .all(db)
            .await?;

    if sensors_with_state.is_empty() {
        tracing::debug!("No active sensors to sync");
        return Ok(());
    }

    // Build a map of vaisala_location_id -> (sensor_id, last_data_time)
    // If force_full_sync is true, we ignore last_data_time to re-fetch everything
    let mut location_map: HashMap<i32, (Uuid, Option<chrono::DateTime<Utc>>)> = HashMap::new();
    for (sensor, state) in &sensors_with_state {
        let last_time = if force_full_sync {
            None
        } else {
            state
                .as_ref()
                .and_then(|s| s.last_data_time.map(|dt| dt.with_timezone(&Utc)))
        };
        location_map.insert(sensor.vaisala_location_id, (sensor.id, last_time));
    }

    // Group by earliest date_from to minimize API calls
    // For initial sync, use max_history_days; for incremental, use last_data_time
    let now = Utc::now();
    let max_history_start = now - Duration::days(max_history_days);

    // Collect all location IDs
    let location_ids: Vec<i32> = location_map.keys().copied().collect();

    // Determine the earliest date_from across all sensors
    let earliest_from = location_map
        .values()
        .map(|(_, last_time)| last_time.unwrap_or(max_history_start))
        .min()
        .unwrap_or(max_history_start);

    tracing::info!(
        sensor_count = location_ids.len(),
        from = %earliest_from,
        "Syncing readings"
    );

    // Fetch history from Vaisala
    let history = match vaisala
        .get_locations_history(&location_ids, earliest_from, Some(now))
        .await
    {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "Failed to fetch locations history");
            // Update sync state with error for all sensors
            for (sensor, _) in &sensors_with_state {
                update_sync_state_error(db, sensor.id, &e.to_string()).await;
            }
            return Err(e);
        }
    };

    // Process each location's samples from JSON API data array
    for resource in history.data {
        let attrs = resource.attributes;
        let Some((sensor_id, last_time)) = location_map.get(&attrs.id) else {
            tracing::warn!(
                location_id = attrs.id,
                "Received data for unknown location"
            );
            continue;
        };

        // Filter data points to only those after last_data_time (if any)
        // Convert epoch timestamps to DateTime for comparison
        let last_timestamp = last_time.map(|lt| lt.timestamp());
        let new_points: Vec<_> = attrs
            .data_points
            .into_iter()
            .filter(|dp| last_timestamp.is_none_or(|lt| dp.timestamp > lt))
            .collect();

        if new_points.is_empty() {
            tracing::debug!(
                sensor_id = %sensor_id,
                location_id = attrs.id,
                "No new samples"
            );
            continue;
        }

        let sample_count = new_points.len();

        // Build all models and track latest timestamp
        let mut models: Vec<readings::ActiveModel> = Vec::with_capacity(new_points.len());
        let mut latest_timestamp: Option<i64> = None;

        for point in new_points {
            // Convert epoch timestamp to DateTime, rounded to nearest 10 minutes.
            // Different sensors report at slightly different times, so rounding
            // aligns them to common timestamps (same approach as the R Shiny portal).
            let raw_time = chrono::DateTime::from_timestamp(point.timestamp, 0)
                .unwrap_or_else(Utc::now);
            let epoch = raw_time.timestamp();
            let rounded_epoch = ((epoch + 300) / 600) * 600; // round to nearest 600s
            let time = chrono::DateTime::from_timestamp(rounded_epoch, 0)
                .unwrap_or(raw_time);

            models.push(readings::ActiveModel {
                sensor_id: Set(*sensor_id),
                time: Set(time.into()),
                value: Set(point.value),
                logged: Set(Some(point.logged)),
            });

            if latest_timestamp.is_none_or(|lt| point.timestamp > lt) {
                latest_timestamp = Some(point.timestamp);
            }
        }

        // Batch insert in chunks of BATCH_SIZE
        for chunk in models.chunks(BATCH_SIZE) {
            if let Err(e) = readings::Entity::insert_many(chunk.to_vec())
                .on_conflict(
                    sea_orm::sea_query::OnConflict::columns([
                        readings::Column::SensorId,
                        readings::Column::Time,
                    ])
                    .do_nothing()
                    .to_owned(),
                )
                .exec(db)
                .await
            {
                // "None of the records are inserted" is expected from ON CONFLICT DO NOTHING
                // when all records in the batch are duplicates
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

        // Update sync state with the latest timestamp
        if let Some(ts) = latest_timestamp
            && let Some(latest) = chrono::DateTime::from_timestamp(ts, 0)
        {
            update_sync_state_success(db, *sensor_id, latest).await;
        }

        tracing::info!(
            count = sample_count,
            sensor_id = %sensor_id,
            location_id = attrs.id,
            "Synced readings"
        );
    }

    Ok(())
}

/// Sync device status for all active sensors.
///
/// # Errors
///
/// Returns an error if the database or Vaisala API operations fail.
pub async fn sync_device_status(db: &DatabaseConnection, vaisala: &VaisalaClient) -> AppResult<()> {
    // Get all active sensors
    let sensors: Vec<sensors::Model> = sensors::Entity::find()
        .filter(sensors::Column::IsActive.eq(true))
        .all(db)
        .await?;

    if sensors.is_empty() {
        tracing::debug!("No active sensors for device status sync");
        return Ok(());
    }

    // Build location_id -> sensor_id map
    let location_map: HashMap<i32, Uuid> = sensors
        .iter()
        .map(|s| (s.vaisala_location_id, s.id))
        .collect();

    let location_ids: Vec<i32> = location_map.keys().copied().collect();

    tracing::info!(sensor_count = location_ids.len(), "Syncing device status");

    // Fetch current data from Vaisala
    let data = vaisala.get_locations_data(&location_ids).await?;

    let now = Utc::now();

    // Insert device status for each location from JSON API data array
    for resource in data.data {
        let attrs = resource.attributes;
        let Some(sensor_id) = location_map.get(&attrs.id) else {
            continue;
        };

        let status = device_status::ActiveModel {
            sensor_id: Set(*sensor_id),
            time: Set(now.into()),
            battery_level: Set(Some(attrs.battery_level)),
            battery_state: Set(Some(attrs.battery_state)),
            signal_quality: Set(Some(attrs.signal_quality)),
            device_status: Set(Some(attrs.device_status)),
            unreachable: Set(Some(attrs.unreachable)),
        };

        if let Err(e) = status.insert(db).await {
            tracing::warn!(
                sensor_id = %sensor_id,
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
    sensor_id: Uuid,
    latest_time: chrono::DateTime<Utc>,
) {
    let state = sync_state::ActiveModel {
        sensor_id: Set(sensor_id),
        last_data_time: Set(Some(latest_time.into())),
        last_sync_attempt: Set(Some(Utc::now().into())),
        sync_status: Set(Some("success".to_string())),
        error_message: Set(None),
        retry_count: Set(Some(0)),
        last_full_sync: sea_orm::ActiveValue::NotSet,
    };

    // Upsert sync state (note: last_full_sync is updated separately by scheduler)
    if let Err(e) = sync_state::Entity::insert(state)
        .on_conflict(
            sea_orm::sea_query::OnConflict::column(sync_state::Column::SensorId)
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
            sensor_id = %sensor_id,
            error = %e,
            "Failed to update sync state"
        );
    }
}

async fn update_sync_state_error(db: &DatabaseConnection, sensor_id: Uuid, error: &str) {
    // First try to get current retry count
    let current = sync_state::Entity::find_by_id(sensor_id)
        .one(db)
        .await
        .ok()
        .flatten();

    let retry_count = current.and_then(|s| s.retry_count).unwrap_or(0) + 1;

    let state = sync_state::ActiveModel {
        sensor_id: Set(sensor_id),
        last_data_time: Set(None),
        last_sync_attempt: Set(Some(Utc::now().into())),
        sync_status: Set(Some("error".to_string())),
        error_message: Set(Some(error.to_string())),
        retry_count: Set(Some(retry_count)),
        last_full_sync: sea_orm::ActiveValue::NotSet,
    };

    if let Err(e) = sync_state::Entity::insert(state)
        .on_conflict(
            sea_orm::sea_query::OnConflict::column(sync_state::Column::SensorId)
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
            sensor_id = %sensor_id,
            error = %e,
            "Failed to update sync state error"
        );
    }
}

/// Update last_full_sync timestamp for all sensors.
/// Called after a successful full re-sync.
pub async fn update_last_full_sync_for_all_sensors(db: &DatabaseConnection) {
    let now = Utc::now();

    // Get all sync states and update their last_full_sync
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
        // No sync state yet, will need full sync on first run
        return true;
    }

    let now = Utc::now();
    let threshold = Duration::hours(24);

    // If any sensor has never had a full sync, or has one older than 24h, do full sync
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

/// Sync active alarms from Vaisala.
///
/// Fetches all active alarms and upserts them into the database.
/// Links alarms to sensors via the alarm_locations junction table.
///
/// # Errors
///
/// Returns an error if the Vaisala API or database operations fail.
pub async fn sync_alarms(db: &DatabaseConnection, vaisala: &VaisalaClient) -> AppResult<()> {
    tracing::info!("Syncing alarms from Vaisala...");

    // Fetch active alarms (include system alarms)
    let response = vaisala.get_active_alarms(None, true).await?;

    // Build sensor lookup by vaisala_location_id (includes station_id for linking)
    let all_sensors = sensors::Entity::find().all(db).await?;
    let sensor_map: HashMap<i32, Uuid> = all_sensors
        .iter()
        .map(|s| (s.vaisala_location_id, s.id))
        .collect();
    let sensor_station_map: HashMap<i32, Uuid> = all_sensors
        .iter()
        .map(|s| (s.vaisala_location_id, s.station_id))
        .collect();

    // Build existing alarms lookup by vaisala_alarm_id
    let existing_alarms: HashMap<i32, alarms::Model> = alarms::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .map(|a| (a.vaisala_alarm_id, a))
        .collect();

    let now = Utc::now();
    let mut created = 0;
    let mut updated = 0;

    // Collect active IDs and total count before consuming the response
    let active_ids: Vec<i32> = response.data.iter().map(|r| r.attributes.id).collect();
    let total_alarms = response.data.len();

    for resource in response.data {
        let attrs = resource.attributes;

        // Convert timestamps
        let when_on = chrono::DateTime::from_timestamp(attrs.when_on as i64, 0)
            .unwrap_or_else(Utc::now);
        let when_off = attrs
            .when_off
            .and_then(|ts| chrono::DateTime::from_timestamp(ts as i64, 0));
        let when_ack = attrs
            .when_ack
            .and_then(|ts| chrono::DateTime::from_timestamp(ts as i64, 0));
        let when_condition = attrs
            .when_condition
            .and_then(|ts| chrono::DateTime::from_timestamp(ts as i64, 0));

        let ack_comments = attrs.ack_comments.map(|c| serde_json::json!(c));

        if let Some(existing) = existing_alarms.get(&attrs.id) {
            // Update existing alarm
            let mut model: alarms::ActiveModel = existing.clone().into();
            model.severity = Set(attrs.severity);
            model.description = Set(attrs.description.clone());
            model.error_text = Set(if attrs.error_text.is_empty() {
                None
            } else {
                Some(attrs.error_text.clone())
            });
            model.when_off = Set(when_off.map(Into::into));
            model.when_ack = Set(when_ack.map(Into::into));
            model.duration_sec = Set(Some(attrs.duration_sec));
            model.status = Set(attrs.status);
            model.ack_comments = Set(ack_comments);
            model.ack_action_taken = Set(attrs.ack_action_taken.clone());
            model.updated_at = Set(Some(now.into()));

            if let Err(e) = model.update(db).await {
                tracing::warn!(
                    error = %e,
                    vaisala_alarm_id = attrs.id,
                    "Failed to update alarm"
                );
            } else {
                updated += 1;
            }
        } else {
            // Derive station_id from the first location_id that maps to a sensor
            let station_id = attrs
                .location_ids
                .iter()
                .find_map(|loc_id| sensor_station_map.get(loc_id).copied());

            // Create new alarm
            let alarm_id = Uuid::new_v4();
            let alarm = alarms::ActiveModel {
                id: Set(alarm_id),
                vaisala_alarm_id: Set(attrs.id),
                severity: Set(attrs.severity),
                description: Set(attrs.description.clone()),
                error_text: Set(if attrs.error_text.is_empty() {
                    None
                } else {
                    Some(attrs.error_text.clone())
                }),
                alarm_type: Set(None), // Could derive from description/error_text if needed
                when_on: Set(when_on.into()),
                when_off: Set(when_off.map(Into::into)),
                when_ack: Set(when_ack.map(Into::into)),
                when_condition: Set(when_condition.map(Into::into)),
                duration_sec: Set(Some(attrs.duration_sec)),
                status: Set(attrs.status),
                is_system: Set(attrs.is_system),
                serial_number: Set(if attrs.serial_number.is_empty() {
                    None
                } else {
                    Some(attrs.serial_number.clone())
                }),
                location_text: Set(if attrs.location.is_empty() {
                    None
                } else {
                    Some(attrs.location.clone())
                }),
                zone_text: Set(if attrs.zone.is_empty() {
                    None
                } else {
                    Some(attrs.zone.clone())
                }),
                station_id: Set(station_id),
                ack_required: Set(attrs.ack_required),
                ack_comments: Set(ack_comments),
                ack_action_taken: Set(attrs.ack_action_taken.clone()),
                created_at: Set(Some(now.into())),
                updated_at: Set(Some(now.into())),
            };

            match alarm.insert(db).await {
                Ok(_) => {
                    created += 1;

                    // Link alarm to sensors via alarm_locations
                    for location_id in &attrs.location_ids {
                        if let Some(sensor_id) = sensor_map.get(location_id) {
                            let link = alarm_locations::ActiveModel {
                                alarm_id: Set(alarm_id),
                                sensor_id: Set(*sensor_id),
                            };
                            if let Err(e) = link.insert(db).await {
                                // Ignore duplicate key errors
                                let msg = e.to_string();
                                if !msg.contains("duplicate") {
                                    tracing::warn!(
                                        error = %e,
                                        alarm_id = %alarm_id,
                                        sensor_id = %sensor_id,
                                        "Failed to link alarm to sensor"
                                    );
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        vaisala_alarm_id = attrs.id,
                        "Failed to create alarm"
                    );
                }
            }
        }
    }

    // Mark alarms as inactive if they're no longer in the active list
    for (vaisala_id, existing) in &existing_alarms {
        if existing.status && !active_ids.contains(vaisala_id) {
            let mut model: alarms::ActiveModel = existing.clone().into();
            model.status = Set(false);
            model.when_off = Set(Some(now.into()));
            model.updated_at = Set(Some(now.into()));

            if let Err(e) = model.update(db).await {
                tracing::warn!(
                    error = %e,
                    vaisala_alarm_id = vaisala_id,
                    "Failed to mark alarm as inactive"
                );
            }
        }
    }

    tracing::info!(
        created,
        updated,
        total = total_alarms,
        "Alarms sync completed"
    );

    Ok(())
}

/// Sync events from Vaisala.
///
/// Fetches recent events (last 7 days by default) and inserts new ones.
/// Links events to sensors when location_id maps to a known sensor.
///
/// # Errors
///
/// Returns an error if the Vaisala API or database operations fail.
pub async fn sync_events(db: &DatabaseConnection, vaisala: &VaisalaClient) -> AppResult<()> {
    tracing::info!("Syncing events from Vaisala...");

    // Get latest event time to only fetch newer events
    let latest_event = events::Entity::find()
        .order_by_desc(events::Column::Time)
        .one(db)
        .await?;

    // Default to 7 days ago if no events exist
    let date_from = match latest_event {
        Some(e) => e.time.with_timezone(&Utc).timestamp().to_string(),
        None => "7d".to_string(),
    };

    // Build sensor lookup (includes station_id for linking)
    let all_sensors_for_events = sensors::Entity::find().all(db).await?;
    let sensor_map_events: HashMap<i32, Uuid> = all_sensors_for_events
        .iter()
        .map(|s| (s.vaisala_location_id, s.id))
        .collect();
    let sensor_station_map_events: HashMap<i32, Uuid> = all_sensors_for_events
        .iter()
        .map(|s| (s.vaisala_location_id, s.station_id))
        .collect();

    // Fetch events in pages
    let mut page = 1;
    let page_size = 1000;
    let mut total_created = 0;

    loop {
        let response = vaisala
            .get_events(&date_from, None, None, None, Some(page), Some(page_size))
            .await?;

        if response.data.is_empty() {
            break;
        }

        for resource in &response.data {
            let attrs = &resource.attributes;

            // Convert timestamp
            let time = chrono::DateTime::from_timestamp(attrs.timestamp as i64, 0)
                .unwrap_or_else(Utc::now);

            // Try to link to sensor and derive station
            let location_id_int = attrs
                .location_id
                .as_ref()
                .and_then(|lid| lid.as_int());
            let sensor_id = location_id_int
                .and_then(|id| sensor_map_events.get(&id).copied());
            let station_id = location_id_int
                .and_then(|id| sensor_station_map_events.get(&id).copied());

            let extra_fields = if attrs.extra_fields.is_empty() {
                None
            } else {
                Some(serde_json::json!(attrs.extra_fields))
            };

            let event = events::ActiveModel {
                time: Set(time.into()),
                vaisala_event_num: Set(attrs.num),
                category: Set(attrs.category.clone()),
                message: Set(attrs.message.clone()),
                user_name: Set(if attrs.user_name.is_empty() {
                    None
                } else {
                    Some(attrs.user_name.clone())
                }),
                entity: Set(if attrs.entity.is_empty() {
                    None
                } else {
                    Some(attrs.entity.clone())
                }),
                entity_id: Set(if attrs.entity_id == 0 {
                    None
                } else {
                    Some(attrs.entity_id)
                }),
                sensor_id: Set(sensor_id),
                station_id: Set(station_id),
                device_id: Set(attrs.device_id),
                channel_id: Set(attrs.channel_id),
                host_id: Set(attrs.host_id),
                extra_fields: Set(extra_fields),
            };

            match event.insert(db).await {
                Ok(_) => total_created += 1,
                Err(e) => {
                    // Ignore duplicate key errors (event already exists)
                    let msg = e.to_string();
                    if !msg.contains("duplicate") && !msg.contains("unique") {
                        tracing::warn!(
                            error = %e,
                            event_num = attrs.num,
                            "Failed to insert event"
                        );
                    }
                }
            }
        }

        // Check if we've fetched all pages
        let meta = response.meta.as_ref();
        let total_records = meta.map(|m| m.total_record_count).unwrap_or(0);
        let fetched = page * page_size;

        if fetched >= total_records || response.data.len() < page_size as usize {
            break;
        }

        page += 1;
    }

    tracing::info!(created = total_created, "Events sync completed");

    Ok(())
}

/// Refresh continuous aggregates after new data is synced.
///
/// Refreshes the hourly aggregate for recent data (last 24 hours).
/// This ensures dashboards show aggregated data promptly without waiting
/// for the scheduled refresh policy.
///
/// Note: Only refreshes hourly; daily/weekly/monthly are less time-sensitive
/// and can rely on their scheduled policies.
pub async fn refresh_continuous_aggregates(db: &DatabaseConnection) {
    tracing::debug!("Refreshing continuous aggregates...");

    // Refresh hourly aggregate for recent data (last 24 hours to now)
    // Using a bounded window is faster than refreshing the entire history
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

    // Also refresh daily for last 7 days (less frequently needed but helps with dashboard)
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
