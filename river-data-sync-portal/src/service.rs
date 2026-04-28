use std::time::Instant;

use sqlx::MySqlPool;

use river_data_sync_common::models::SyncResult;
use river_data_sync_common::river_data_client::RiverDataClient;
use river_data_sync_common::runner::SyncService;

use crate::backend::PortalBackend;
use crate::config::PortalConfig;
use crate::sync;

pub struct PortalSyncService {
    config: PortalConfig,
    api: RiverDataClient,
    pool: MySqlPool,
    backend: Box<dyn PortalBackend>,
}

impl PortalSyncService {
    pub fn new(
        config: PortalConfig,
        api: RiverDataClient,
        pool: MySqlPool,
        backend: Box<dyn PortalBackend>,
    ) -> Self {
        Self {
            config,
            api,
            pool,
            backend,
        }
    }
}

#[async_trait::async_trait]
impl SyncService for PortalSyncService {
    fn service_type(&self) -> &str {
        self.backend.source_system()
    }

    async fn sync(
        &self,
        full: bool,
    ) -> Result<SyncResult, Box<dyn std::error::Error + Send + Sync>> {
        let start = Instant::now();
        let mut errors = Vec::new();
        let mut log = Vec::new();

        // Discover streams (idempotent)
        match sync::discover_streams(self.backend.as_ref(), &self.pool, &self.api).await {
            Ok(count) => log.push(format!("Stream discovery: {count} streams registered")),
            Err(e) => {
                tracing::error!(error = %e, "Failed to discover streams");
                errors.push(format!("Stream discovery: {e}"));
            }
        }

        // Sync readings with retry
        let mut retries = 0u32;
        let mut readings_synced: u64 = 0;

        let sync_ok = loop {
            match sync::sync_readings(self.backend.as_ref(), &self.pool, &self.api, full).await {
                Ok(summary) => {
                    readings_synced = summary.total_readings as u64;
                    log.push(format!(
                        "Readings sync: {} readings across {} streams",
                        summary.total_readings, summary.streams_synced
                    ));
                    break true;
                }
                Err(e) => {
                    retries += 1;
                    if retries <= self.config.retry_max {
                        tracing::warn!(
                            error = %e,
                            retry = retries,
                            max = self.config.retry_max,
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
                        tracing::error!(error = %e, "Readings sync failed after max retries");
                        errors.push(format!("Readings sync: {e}"));
                        break false;
                    }
                }
            }
        };

        // Refresh aggregates if readings sync succeeded
        if sync_ok && readings_synced > 0 {
            match self.api.refresh_aggregates(full).await {
                Ok(()) => log.push(format!("Aggregate refresh (full={full}): OK")),
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to refresh aggregates");
                    errors.push(format!("Aggregate refresh: {e}"));
                }
            }
        }

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
            status_events_synced: 0,
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
