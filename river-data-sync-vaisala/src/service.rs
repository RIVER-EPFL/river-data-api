use std::time::Instant;

use river_data_sync_common::models::SyncResult;
use river_data_sync_common::river_data_client::RiverDataClient;
use river_data_sync_common::runner::SyncService;

use crate::config::SyncConfig;
use crate::sync;
use crate::vaisala_client::VaisalaClient;

pub struct VaisalaSyncService {
    config: SyncConfig,
    api: RiverDataClient,
    vaisala: VaisalaClient,
}

impl VaisalaSyncService {
    pub fn new(config: SyncConfig, api: RiverDataClient, vaisala: VaisalaClient) -> Self {
        Self {
            config,
            api,
            vaisala,
        }
    }
}

#[async_trait::async_trait]
impl SyncService for VaisalaSyncService {
    fn service_type(&self) -> &str {
        "vaisala"
    }

    async fn sync(
        &self,
        full: bool,
    ) -> Result<SyncResult, Box<dyn std::error::Error + Send + Sync>> {
        let start = Instant::now();
        let mut errors = Vec::new();
        let mut log = Vec::new();

        // Discover streams on every sync cycle (idempotent)
        match sync::discover_streams(&self.api, &self.vaisala).await {
            Ok(stream_map) => log.push(format!(
                "Stream discovery: {} streams registered",
                stream_map.len()
            )),
            Err(e) => {
                tracing::error!(error = %e, "Failed to discover streams from Vaisala");
                errors.push(format!("Stream discovery: {e}"));
            }
        }

        // Run readings sync with retry
        let mut retries = 0u32;
        let mut readings_synced: u64 = 0;

        let sync_ok = loop {
            match sync::sync_readings(
                &self.api,
                &self.vaisala,
                self.config.max_history_days,
                full,
            )
            .await
            {
                Ok(summary) => {
                    readings_synced = summary.total_readings as u64;
                    log.push(format!(
                        "Readings sync: {} readings across {} streams",
                        summary.total_readings, summary.streams_synced
                    ));
                    for entry in &summary.per_stream {
                        log.push(format!("  {entry}"));
                    }
                    break true;
                }
                Err(e) => {
                    retries += 1;
                    if retries <= self.config.retry_max {
                        tracing::warn!(
                            error = %e,
                            retry = retries,
                            max_retries = self.config.retry_max,
                            "Readings sync failed, retrying"
                        );
                        log.push(format!(
                            "Readings sync: retry {retries}/{} - {e}",
                            self.config.retry_max
                        ));
                        tokio::time::sleep(std::time::Duration::from_secs(
                            self.config.retry_delay_seconds,
                        ))
                        .await;
                    } else {
                        tracing::error!(
                            error = %e,
                            max_retries = self.config.retry_max,
                            "Readings sync failed after max retries"
                        );
                        errors.push(format!("Readings sync: {e}"));
                        break false;
                    }
                }
            }
        };

        if sync_ok {
            match self.api.refresh_aggregates(full).await {
                Ok(()) => log.push(format!("Aggregate refresh (full={full}): OK")),
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to trigger aggregate refresh");
                    errors.push(format!("Aggregate refresh: {e}"));
                }
            }
        }

        // Run device status sync (non-fatal)
        let status_count = match sync::sync_device_status(&self.api, &self.vaisala).await {
            Ok(n) => {
                log.push(format!("Device status sync: {n} events"));
                n
            }
            Err(e) => {
                tracing::warn!(error = %e, "Device status sync failed");
                errors.push(format!("Device status sync: {e}"));
                0
            }
        };

        // If readings sync completely failed, return error
        if !sync_ok {
            let elapsed = start.elapsed();
            return Err(format!(
                "Readings sync failed after {} retries ({}ms): {}",
                self.config.retry_max,
                elapsed.as_millis(),
                errors.join("; ")
            )
            .into());
        }

        let elapsed = start.elapsed();
        Ok(SyncResult {
            readings_synced,
            status_events_synced: status_count,
            full_sync: full,
            duration_ms: elapsed.as_millis() as u64,
            errors,
            log,
        })
    }

    fn update_token(&self, token: &str) {
        self.api.set_token(token);
    }

    fn river_data_client(&self) -> Option<&RiverDataClient> {
        Some(&self.api)
    }
}
