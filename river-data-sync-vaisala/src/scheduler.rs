use std::time::Duration;
use tokio::time::interval;

use crate::api_client::ApiClient;
use crate::config::SyncConfig;
use crate::sync;
use crate::vaisala_client::VaisalaClient;

/// Run the readings sync task on a schedule.
pub async fn run_sync(config: &SyncConfig, api: &ApiClient, vaisala: &VaisalaClient) {
    tracing::info!(
        interval_secs = config.sync_interval_seconds,
        max_history_days = config.max_history_days,
        "Starting readings sync scheduler"
    );

    // Discover locations on startup
    if let Err(e) = sync::sync_locations(api, vaisala).await {
        tracing::error!(error = %e, "Failed to discover locations from Vaisala");
    }

    let mut ticker = interval(Duration::from_secs(config.sync_interval_seconds));
    ticker.tick().await; // First tick fires immediately

    loop {
        let force_full_sync = needs_full_sync(api).await;

        if force_full_sync {
            tracing::info!("Triggering full re-sync (24h periodic or initial sync)");
        } else {
            tracing::debug!("Running incremental readings sync...");
        }

        let mut retries = 0u32;
        let mut sync_succeeded = false;

        loop {
            match sync::sync_readings(api, vaisala, config.max_history_days, force_full_sync).await
            {
                Ok(()) => {
                    sync_succeeded = true;
                    if force_full_sync {
                        tracing::info!("Full re-sync completed successfully");
                    } else {
                        tracing::debug!("Readings sync completed successfully");
                    }
                    break;
                }
                Err(e) => {
                    retries += 1;
                    if retries <= config.retry_max {
                        tracing::warn!(
                            error = %e,
                            retry = retries,
                            max_retries = config.retry_max,
                            "Readings sync failed, retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(config.retry_delay_seconds)).await;
                    } else {
                        tracing::error!(
                            error = %e,
                            max_retries = config.retry_max,
                            "Readings sync failed after max retries"
                        );
                        break;
                    }
                }
            }
        }

        if sync_succeeded {
            if force_full_sync {
                if let Err(e) = api.update_last_full_sync().await {
                    tracing::warn!(error = %e, "Failed to update last_full_sync");
                }
            }
            if let Err(e) = api.refresh_aggregates(force_full_sync).await {
                tracing::warn!(error = %e, "Failed to trigger aggregate refresh");
            }
        }

        ticker.tick().await;
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
