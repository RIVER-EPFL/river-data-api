use chrono::{Duration, Utc};
use std::collections::HashMap;
use uuid::Uuid;

use river_data_sync_common::models::{
    DataStream, IngestReading, IngestStatusEvent, RegisterStreamRequest,
};
use river_data_sync_common::river_data_client::RiverDataClient;

use crate::vaisala_client::{SyncError, VaisalaClient};

const BATCH_SIZE: usize = 1000;

/// Discover locations from Vaisala and register them as data streams.
///
/// No entity creation (projects, sites, parameters, sensors, calibrations, etc.)
/// — sync services only register streams and push data.
pub async fn discover_streams(
    api: &RiverDataClient,
    vaisala: &VaisalaClient,
) -> Result<HashMap<i32, Uuid>, SyncError> {
    tracing::info!("Discovering streams from Vaisala...");

    let locations = vaisala.get_locations().await?;
    let mut stream_map: HashMap<i32, Uuid> = HashMap::new();

    for resource in &locations.data {
        let attrs = &resource.attributes;
        if attrs.deleted || !attrs.leaf {
            continue;
        }

        let location_key = attrs.node_id.to_string();
        // Extract leaf name from path (last segment)
        let leaf_name = attrs.path.split('/').last().unwrap_or(&attrs.text);

        let req = RegisterStreamRequest {
            source_system: "vaisala".to_string(),
            source_key: location_key.clone(),
            source_name: Some(leaf_name.to_string()),
            source_path: Some(attrs.path.clone()),
            metadata: serde_json::json!({
                "vaisala_node_id": attrs.node_id,
            }),
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
                sensor_id: None,
                calibration_id: None,
                deployment_id: None,
            });
        }

        // Insert in batches
        let mut actually_inserted: usize = 0;
        for chunk in readings.chunks(BATCH_SIZE) {
            match api.ingest_readings(*stream_id, chunk).await {
                Ok(n) => actually_inserted += n as usize,
                Err(e) => tracing::warn!(
                    error = %e,
                    batch_size = chunk.len(),
                    "Failed to ingest reading batch"
                ),
            }
        }

        tracing::info!(
            new = actually_inserted,
            total = sample_count,
            stream_id = %stream_id,
            location_id = attrs.id,
            "Synced readings"
        );

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
