use std::time::Instant;

use river_data_sync_common::models::SyncResult;
use river_data_sync_common::runner::SyncService;

use crate::api_client::ApiClient;
use crate::config::SyncConfig;
use crate::sync;
use crate::vaisala_client::VaisalaClient;

pub struct VaisalaSyncService {
    config: SyncConfig,
    api: ApiClient,
    vaisala: VaisalaClient,
}

impl VaisalaSyncService {
    pub fn new(config: SyncConfig, api: ApiClient, vaisala: VaisalaClient) -> Self {
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

        // Discover locations on every sync cycle (idempotent)
        if let Err(e) = sync::sync_locations(&self.api, &self.vaisala).await {
            tracing::error!(error = %e, "Failed to discover locations from Vaisala");
        }

        let force_full = if full {
            true
        } else {
            needs_full_sync(&self.api).await
        };

        if force_full {
            tracing::info!("Running full re-sync");
        }

        // Run readings sync with retry
        let mut retries = 0u32;

        let sync_ok = loop {
            match sync::sync_readings(
                &self.api,
                &self.vaisala,
                self.config.max_history_days,
                force_full,
            )
            .await
            {
                Ok(()) => break true,
                Err(e) => {
                    retries += 1;
                    if retries <= self.config.retry_max {
                        tracing::warn!(
                            error = %e,
                            retry = retries,
                            max_retries = self.config.retry_max,
                            "Readings sync failed, retrying"
                        );
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
                        return Err(e.into());
                    }
                }
            }
        };

        if sync_ok {
            if force_full {
                if let Err(e) = self.api.update_last_full_sync().await {
                    tracing::warn!(error = %e, "Failed to update last_full_sync");
                }
            }
            if let Err(e) = self.api.refresh_aggregates(force_full).await {
                tracing::warn!(error = %e, "Failed to trigger aggregate refresh");
            }
        }

        // Run device status sync (non-fatal)
        if let Err(e) = sync::sync_device_status(&self.api, &self.vaisala).await {
            tracing::warn!(error = %e, "Device status sync failed");
        }

        let elapsed = start.elapsed();
        Ok(SyncResult {
            readings_synced: 0,
            status_events_synced: 0,
            full_sync: force_full,
            duration_ms: elapsed.as_millis() as u64,
        })
    }

    fn update_token(&self, token: &str) {
        self.api.set_token(token);
    }
}

/// Check if any sync state needs a full re-sync (> 24 hours since last full sync).
async fn needs_full_sync(api: &ApiClient) -> bool {
    let states = match api.list_sync_states().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to check full sync status, assuming needed");
            return true;
        }
    };

    if states.is_empty() {
        return true;
    }

    let now = chrono::Utc::now();
    let threshold = chrono::Duration::hours(24);

    for state in states {
        match state.last_full_sync {
            None => return true,
            Some(last) => {
                if now - last > threshold {
                    return true;
                }
            }
        }
    }

    false
}
