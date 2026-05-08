use chrono::{Duration, Utc};
use std::collections::HashMap;
use uuid::Uuid;

use river_data_sync_common::models::{
    DataStream, IngestReading, IngestStatusEvent, RegisterStreamRequest,
};
use river_data_sync_common::river_data_client::RiverDataClient;

use crate::vaisala_client::{SyncError, VaisalaClient};

const BATCH_SIZE: usize = 1000;

fn parse_hierarchy(path: &str) -> serde_json::Value {
    let segments: Vec<&str> = path.split('/').collect();
    serde_json::json!({
        "project": segments.get(1).unwrap_or(&""),
        "site": segments.get(2).unwrap_or(&""),
        "parameter": segments.get(3).unwrap_or(&""),
    })
}

/// Discover locations from Vaisala and register them as data streams.
pub async fn discover_streams(
    api: &RiverDataClient,
    vaisala: &VaisalaClient,
) -> Result<HashMap<i32, Uuid>, SyncError> {
    tracing::info!("Discovering streams from Vaisala...");

    let locations = vaisala.get_locations().await?;

    let leaf_ids: Vec<i32> = locations
        .data
        .iter()
        .filter(|r| !r.attributes.deleted && r.attributes.leaf)
        .map(|r| r.attributes.node_id)
        .collect();

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
        let leaf_name = attrs.path.split('/').last().unwrap_or(&attrs.text);

        let mut metadata = serde_json::json!({
            "vaisala_node_id": attrs.node_id,
            "hierarchy": parse_hierarchy(&attrs.path),
        });

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

pub struct ReadingsSyncSummary {
    pub streams_synced: usize,
    pub total_readings: usize,
    pub per_stream: Vec<String>,
}

/// Sync readings for all active Vaisala streams.
///
/// Splits streams into two groups to avoid one new stream dragging all others
/// back to the max history window:
/// - **Backfill**: streams with no `last_data_time` (new or never synced) →
///   fetch from `max_history_days` ago
/// - **Incremental**: streams with `last_data_time` → fetch from the earliest
///   cursor across the group (typically minutes ago)
pub async fn sync_readings(
    api: &RiverDataClient,
    vaisala: &VaisalaClient,
    max_history_days: i64,
    force_full_sync: bool,
) -> Result<ReadingsSyncSummary, SyncError> {
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

    // Build per-stream state and split into groups
    let mut location_map: HashMap<i32, (Uuid, Option<chrono::DateTime<Utc>>)> = HashMap::new();
    let mut backfill_locations: Vec<i32> = Vec::new();
    let mut incremental_locations: Vec<i32> = Vec::new();

    for stream in &streams {
        if let Ok(loc_id) = stream.source_key.parse::<i32>() {
            let last_time = if force_full_sync {
                None
            } else {
                stream.last_data_time
            };
            location_map.insert(loc_id, (stream.id, last_time));

            match last_time {
                Some(_) => incremental_locations.push(loc_id),
                None => backfill_locations.push(loc_id),
            }
        }
    }

    if location_map.is_empty() {
        return Ok(ReadingsSyncSummary {
            streams_synced: 0,
            total_readings: 0,
            per_stream: vec!["No mapped streams".to_string()],
        });
    }

    let mut total_readings_synced: usize = 0;
    let mut streams_synced: usize = 0;
    let mut per_stream_log: Vec<String> = Vec::new();

    // Group 1: Backfill — new streams or full sync, fetch from max_history_start
    if !backfill_locations.is_empty() {
        tracing::info!(
            count = backfill_locations.len(),
            from = %max_history_start,
            "Backfilling streams with no prior data"
        );

        let history = vaisala
            .get_locations_history(&backfill_locations, max_history_start, Some(now))
            .await?;

        let (synced, readings, logs) =
            process_history(api, history, &location_map).await;
        streams_synced += synced;
        total_readings_synced += readings;
        per_stream_log.extend(logs);
    }

    // Group 2: Incremental — fetch from the earliest cursor in the group
    if !incremental_locations.is_empty() {
        let earliest_cursor = incremental_locations
            .iter()
            .filter_map(|loc| location_map.get(loc).and_then(|(_, lt)| *lt))
            .min()
            .unwrap_or(now);

        tracing::info!(
            count = incremental_locations.len(),
            from = %earliest_cursor,
            "Incremental sync from last known data"
        );

        let history = vaisala
            .get_locations_history(&incremental_locations, earliest_cursor, Some(now))
            .await?;

        let (synced, readings, logs) =
            process_history(api, history, &location_map).await;
        streams_synced += synced;
        total_readings_synced += readings;
        per_stream_log.extend(logs);
    }

    if backfill_locations.is_empty() && incremental_locations.is_empty() {
        per_stream_log.push("All streams up to date".to_string());
    }

    Ok(ReadingsSyncSummary {
        streams_synced,
        total_readings: total_readings_synced,
        per_stream: per_stream_log,
    })
}

/// Process a Vaisala history response: filter per-stream by cursor, ingest readings.
async fn process_history(
    api: &RiverDataClient,
    history: crate::models::LocationsHistoryResponse,
    location_map: &HashMap<i32, (Uuid, Option<chrono::DateTime<Utc>>)>,
) -> (usize, usize, Vec<String>) {
    let mut streams_synced: usize = 0;
    let mut total_readings: usize = 0;
    let mut log: Vec<String> = Vec::new();

    for resource in history.data {
        let attrs = resource.attributes;
        let Some((stream_id, last_time)) = location_map.get(&attrs.id) else {
            continue;
        };

        // Per-stream filtering: only keep points newer than this stream's cursor
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
        total_readings += actually_inserted;
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
        log.push(detail);
    }

    (streams_synced, total_readings, log)
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

    let mut total_inserted: u64 = 0;
    let mut seen_locations: std::collections::HashSet<i32> = std::collections::HashSet::new();

    for resource in data.data {
        let attrs = resource.attributes;

        if !seen_locations.insert(attrs.id) {
            continue;
        }

        let Some(measurement_stream) = stream_map.get(&attrs.id) else {
            continue;
        };

        let events: Vec<IngestStatusEvent> = vec![IngestStatusEvent {
            time: now,
            value: format!(
                "status={} battery={} signal={} powered={} unreachable={}",
                attrs.device_status,
                attrs.battery_level,
                attrs.signal_quality,
                attrs.line_powered,
                attrs.unreachable
            ),
        }];

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
