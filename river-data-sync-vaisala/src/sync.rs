use chrono::{Duration, Utc};
use std::collections::HashMap;
use uuid::Uuid;

use river_data_sync_common::models::{
    DataStream, IngestReading, IngestStatusEvent, RegisterStreamRequest,
};
use river_data_sync_common::river_data_client::RiverDataClient;

use crate::vaisala_client::{SyncError, VaisalaClient};

const BATCH_SIZE: usize = 1000;

/// Parse a Vaisala source path into hierarchy components.
///
/// Path format: `viewLinc/PROJECT/SITE/PARAMETER`
/// Returns (project, site, parameter) names extracted from the path.
fn parse_hierarchy(path: &str) -> serde_json::Value {
    let segments: Vec<&str> = path.split('/').collect();
    // [0]="viewLinc" (skip), [1]=project, [2]=site, [3]=parameter
    serde_json::json!({
        "project": segments.get(1).unwrap_or(&""),
        "site": segments.get(2).unwrap_or(&""),
        "parameter": segments.get(3).unwrap_or(&""),
    })
}

/// Discover locations from Vaisala and register them as data streams.
///
/// No entity creation (projects, sites, parameters, sensors, calibrations, etc.)
/// — sync services only register streams and push data.
///
/// Enriches stream metadata with hierarchy (project/site/parameter parsed from
/// source_path), device info (serial numbers), units, and sample interval from
/// the Vaisala `locations_data` endpoint.
pub async fn discover_streams(
    api: &RiverDataClient,
    vaisala: &VaisalaClient,
) -> Result<HashMap<i32, Uuid>, SyncError> {
    tracing::info!("Discovering streams from Vaisala...");

    let locations = vaisala.get_locations().await?;

    // Collect leaf location IDs for the locations_data call
    let leaf_ids: Vec<i32> = locations
        .data
        .iter()
        .filter(|r| !r.attributes.deleted && r.attributes.leaf)
        .map(|r| r.attributes.node_id)
        .collect();

    // Fetch device metadata for all leaf locations
    let location_data_map: HashMap<i32, _> = if !leaf_ids.is_empty() {
        match vaisala.get_locations_data(&leaf_ids).await {
            Ok(data) => data
                .data
                .into_iter()
                .map(|r| (r.attributes.id, r.attributes))
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "Failed to fetch locations_data, proceeding without device metadata");
                HashMap::new()
            }
        }
    } else {
        HashMap::new()
    };

    let mut stream_map: HashMap<i32, Uuid> = HashMap::new();

    for resource in &locations.data {
        let attrs = &resource.attributes;
        if attrs.deleted || !attrs.leaf {
            continue;
        }

        let location_key = attrs.node_id.to_string();
        // Extract leaf name from path (last segment)
        let leaf_name = attrs.path.split('/').last().unwrap_or(&attrs.text);

        // Build enriched metadata
        let hierarchy = parse_hierarchy(&attrs.path);
        let mut metadata = serde_json::json!({
            "vaisala_node_id": attrs.node_id,
            "hierarchy": hierarchy,
        });

        // Merge device metadata from locations_data if available
        if let Some(ld) = location_data_map.get(&attrs.node_id) {
            metadata["device"] = serde_json::json!({
                "logger_serial": &ld.logger_serial_number,
                "probe_serial": &ld.probe_serial_number,
                "logger_device": &ld.logger_device,
                "device_class": &ld.device_class,
            });
            metadata["units"] = serde_json::json!(&ld.display_units);
            metadata["sample_interval_sec"] = serde_json::json!(ld.sample_interval_sec);
            metadata["channel_id"] = serde_json::json!(ld.channel_id);
        }

        let req = RegisterStreamRequest {
            source_system: "vaisala".to_string(),
            source_key: location_key.clone(),
            source_name: Some(leaf_name.to_string()),
            source_path: Some(attrs.path.clone()),
            metadata,
        };

        match api.register_stream(&req).await {
            Ok(stream) => {
                stream_map.insert(attrs.node_id, stream.id);
                tracing::debug!(
                    node_id = attrs.node_id,
                    stream_id = %stream.id,
                    name = leaf_name,
                    "Registered stream"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, node_id = attrs.node_id, "Failed to register stream");
            }
        }
    }

    tracing::info!(count = stream_map.len(), "Stream discovery complete");
    Ok(stream_map)
}

/// Summary of a readings sync pass.
pub struct ReadingsSyncSummary {
    pub streams_synced: usize,
    pub total_readings: usize,
    pub per_stream: Vec<String>,
}

/// Sync readings for all active Vaisala streams via the ingest endpoint.
pub async fn sync_readings(
    api: &RiverDataClient,
    vaisala: &VaisalaClient,
    max_history_days: i64,
    force_full_sync: bool,
) -> Result<ReadingsSyncSummary, SyncError> {
    // List active vaisala streams
    let streams = api.list_streams(Some("vaisala"), Some(true)).await?;

    if streams.is_empty() {
        tracing::debug!("No active Vaisala streams to sync");
        return Ok(ReadingsSyncSummary {
            streams_synced: 0,
            total_readings: 0,
            per_stream: vec!["No active streams".to_string()],
        });
    }

    let now = Utc::now();
    let max_history_start = now - Duration::days(max_history_days);

    // Build location_id -> (stream_id, last_data_time) map
    let mut location_map: HashMap<i32, (Uuid, Option<chrono::DateTime<Utc>>)> = HashMap::new();

    for stream in &streams {
        if let Ok(loc_id) = stream.source_key.parse::<i32>() {
            let last_time = if force_full_sync {
                None
            } else {
                stream.last_data_time
            };
            location_map.insert(loc_id, (stream.id, last_time));
        }
    }

    if location_map.is_empty() {
        return Ok(ReadingsSyncSummary {
            streams_synced: 0,
            total_readings: 0,
            per_stream: vec!["No mapped streams".to_string()],
        });
    }

    let location_ids: Vec<i32> = location_map.keys().copied().collect();
    let earliest_from = location_map
        .values()
        .map(|(_, last_time)| last_time.unwrap_or(max_history_start))
        .min()
        .unwrap_or(max_history_start);

    tracing::info!(
        stream_count = location_ids.len(),
        from = %earliest_from,
        "Syncing readings"
    );

    let history = vaisala
        .get_locations_history(&location_ids, earliest_from, Some(now))
        .await?;

    let mut total_readings_synced: usize = 0;
    let mut streams_synced: usize = 0;
    let mut per_stream_log: Vec<String> = Vec::new();

    for resource in history.data {
        let attrs = resource.attributes;
        let Some((stream_id, last_time)) = location_map.get(&attrs.id) else {
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
        let mut readings: Vec<IngestReading> = Vec::with_capacity(new_points.len());

        for point in &new_points {
            let raw_time =
                chrono::DateTime::from_timestamp(point.timestamp, 0).unwrap_or_else(Utc::now);
            let epoch = raw_time.timestamp();
            let rounded_epoch = ((epoch + 300) / 600) * 600;
            let time = chrono::DateTime::from_timestamp(rounded_epoch, 0).unwrap_or(raw_time);

            readings.push(IngestReading {
                time,
                raw_value: point.value,
                replicate_index: 0,
                sensor_id: None,
                calibration_id: None,
                deployment_id: None,
            });
        }

        // Insert in batches
        let mut actually_inserted: usize = 0;
        let mut failed_batches: usize = 0;
        for chunk in readings.chunks(BATCH_SIZE) {
            match api.ingest_readings(*stream_id, chunk).await {
                Ok(n) => actually_inserted += n as usize,
                Err(e) => {
                    failed_batches += 1;
                    tracing::warn!(
                        error = %e,
                        batch_size = chunk.len(),
                        "Failed to ingest reading batch"
                    );
                }
            }
        }

        if failed_batches > 0 {
            tracing::warn!(
                inserted = actually_inserted,
                failed_batches,
                total = sample_count,
                stream_id = %stream_id,
                "Partial sync failure: some batches failed"
            );
        } else {
            tracing::info!(
                new = actually_inserted,
                total = sample_count,
                stream_id = %stream_id,
                location_id = attrs.id,
                "Synced readings"
            );
        }

        let is_backfill = last_time.is_none();
        total_readings_synced += actually_inserted;
        streams_synced += 1;
        let duplicates = sample_count - actually_inserted;
        let mut detail = format!(
            "loc {} ({}): {} new readings",
            attrs.id,
            &stream_id.to_string()[..8],
            actually_inserted,
        );
        if duplicates > 0 {
            detail.push_str(&format!(" ({duplicates} duplicates skipped)"));
        }
        if is_backfill {
            detail.push_str(" (backfill)");
        }
        per_stream_log.push(detail);
    }

    Ok(ReadingsSyncSummary {
        streams_synced,
        total_readings: total_readings_synced,
        per_stream: per_stream_log,
    })
}

/// Sync device status from Vaisala into status_events via streams.
pub async fn sync_device_status(
    api: &RiverDataClient,
    vaisala: &VaisalaClient,
) -> Result<u64, SyncError> {
    let streams = api.list_streams(Some("vaisala"), Some(true)).await?;

    if streams.is_empty() {
        tracing::debug!("No active Vaisala streams for device status sync");
        return Ok(0);
    }

    // Parse stream source_keys back to location_ids
    let stream_map: HashMap<i32, &DataStream> = streams
        .iter()
        .filter_map(|s| s.source_key.parse::<i32>().ok().map(|k| (k, s)))
        .collect();

    let location_ids: Vec<i32> = stream_map.keys().copied().collect();

    tracing::info!(
        location_count = location_ids.len(),
        "Syncing device status"
    );

    let data = vaisala.get_locations_data(&location_ids).await?;
    let now = Utc::now();

    // For device status, we need a separate stream per health metric.
    // We'll register streams with source_key like "health:{location_id}:{metric}"
    let mut total_inserted: u64 = 0;
    let mut seen_locations: std::collections::HashSet<i32> = std::collections::HashSet::new();

    for resource in data.data {
        let attrs = resource.attributes;

        // Only process each location once
        if !seen_locations.insert(attrs.id) {
            continue;
        }

        let Some(measurement_stream) = stream_map.get(&attrs.id) else {
            continue;
        };

        // Use the measurement stream for device status events too
        // (status events are per-stream, and this keeps it simple)
        let events: Vec<IngestStatusEvent> = vec![
            IngestStatusEvent {
                time: now,
                value: format!(
                    "status={} battery={} signal={} powered={} unreachable={}",
                    attrs.device_status,
                    attrs.battery_level,
                    attrs.signal_quality,
                    attrs.line_powered,
                    attrs.unreachable
                ),
            },
        ];

        match api
            .ingest_status_events(measurement_stream.id, &events)
            .await
        {
            Ok(n) => total_inserted += n,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    location_id = attrs.id,
                    "Failed to ingest device status"
                );
            }
        }
    }

    tracing::info!(inserted = total_inserted, "Device status sync complete");
    Ok(total_inserted)
}
