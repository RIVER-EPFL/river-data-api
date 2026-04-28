use sqlx::MySqlPool;

use river_data_sync_common::models::{IngestReading, RegisterStreamRequest};
use river_data_sync_common::river_data_client::RiverDataClient;

use crate::backend::{PortalBackend, StreamFetchRequest};
use crate::error::SyncError;

const BATCH_SIZE: usize = 1000;

/// Discover streams from the portal and register them with river-data.
pub async fn discover_streams(
    backend: &dyn PortalBackend,
    pool: &MySqlPool,
    api: &RiverDataClient,
) -> Result<usize, SyncError> {
    let source_system = backend.source_system();
    tracing::info!(source_system, "Discovering streams from portal...");

    let descriptors = backend.discover_stream_descriptors(pool).await?;
    let mut registered = 0;

    for desc in &descriptors {
        let req = RegisterStreamRequest {
            source_system: source_system.to_string(),
            source_key: desc.source_key.clone(),
            source_name: Some(desc.source_name.clone()),
            source_path: Some(desc.source_path.clone()),
            metadata: desc.metadata.clone(),
        };

        match api.register_stream(&req).await {
            Ok(_stream) => registered += 1,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    source_key = %desc.source_key,
                    "Failed to register stream"
                );
            }
        }
    }

    tracing::info!(
        registered,
        total = descriptors.len(),
        "Stream discovery complete"
    );
    Ok(registered)
}

/// Summary of a readings sync pass.
pub struct ReadingsSyncSummary {
    pub streams_synced: usize,
    pub total_readings: usize,
}

/// Sync readings from the portal into river-data.
pub async fn sync_readings(
    backend: &dyn PortalBackend,
    pool: &MySqlPool,
    api: &RiverDataClient,
    force_full_sync: bool,
) -> Result<ReadingsSyncSummary, SyncError> {
    let source_system = backend.source_system();

    // List active streams for this source system
    let streams = api
        .list_streams(Some(source_system), Some(true))
        .await
        .map_err(|e| SyncError::Api(e.to_string()))?;

    if streams.is_empty() {
        tracing::debug!(source_system, "No active streams to sync");
        return Ok(ReadingsSyncSummary {
            streams_synced: 0,
            total_readings: 0,
        });
    }

    // Build fetch requests from stream state
    let fetch_requests: Vec<StreamFetchRequest> = streams
        .iter()
        .map(|s| StreamFetchRequest {
            source_key: s.source_key.clone(),
            stream_id: s.id,
            since: if force_full_sync {
                None
            } else {
                s.last_data_time
            },
        })
        .collect();

    tracing::info!(
        streams = fetch_requests.len(),
        force_full = force_full_sync,
        "Syncing readings"
    );

    // Fetch readings from portal
    let stream_readings = backend.fetch_readings(pool, &fetch_requests).await?;

    // Ingest into river-data
    let mut total_inserted: usize = 0;
    let mut streams_synced: usize = 0;

    for sr in &stream_readings {
        if sr.readings.is_empty() {
            continue;
        }

        let ingest_readings: Vec<IngestReading> = sr
            .readings
            .iter()
            .map(|r| IngestReading {
                time: r.time,
                raw_value: r.value,
                replicate_index: r.replicate_index as i16,
                sensor_id: None,
                calibration_id: None,
                deployment_id: None,
            })
            .collect();

        let mut inserted_for_stream: usize = 0;
        for chunk in ingest_readings.chunks(BATCH_SIZE) {
            match api.ingest_readings(sr.stream_id, chunk).await {
                Ok(n) => inserted_for_stream += n as usize,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        stream_id = %sr.stream_id,
                        batch_size = chunk.len(),
                        "Failed to ingest reading batch"
                    );
                }
            }
        }

        if inserted_for_stream > 0 {
            streams_synced += 1;
            total_inserted += inserted_for_stream;
            tracing::debug!(
                stream = %sr.source_key,
                inserted = inserted_for_stream,
                total = sr.readings.len(),
                "Synced stream"
            );
        }
    }

    tracing::info!(
        streams_synced,
        total_inserted,
        "Readings sync complete"
    );

    Ok(ReadingsSyncSummary {
        streams_synced,
        total_readings: total_inserted,
    })
}
