use std::time::Duration;
use tokio::time::interval;

use super::state;
use super::sync as vaisala_sync;
use crate::common::AppState;

/// Run the readings sync task on a schedule.
///
/// On startup, first discovers locations (projects/sites/parameters) from Vaisala,
/// then performs incremental syncs every interval, with a full re-sync every 24 hours.
pub async fn run_readings_sync(state: AppState) {
    let Some(ref vaisala_client) = state.vaisala_client else {
        tracing::warn!("Vaisala client not configured, readings sync disabled");
        return;
    };

    let interval_secs = state.config.sync_readings_interval_seconds;
    let max_history_days = state
        .config
        .vaisala
        .as_ref()
        .map(|v| v.max_history_days)
        .unwrap_or(90);
    let retry_delay_secs = state.config.sync_retry_delay_seconds;
    let max_retries = state.config.sync_retry_max;

    tracing::info!(
        interval_secs,
        max_history_days,
        "Starting readings sync scheduler"
    );

    // Discover locations from Vaisala on startup
    if let Err(e) = vaisala_sync::sync_locations(&state.db, vaisala_client).await {
        tracing::error!(error = %e, "Failed to discover locations from Vaisala");
    }

    let mut ticker = interval(Duration::from_secs(interval_secs));

    // Run initial sync immediately
    ticker.tick().await;

    loop {
        let force_full_sync = state::needs_full_sync(&state.db).await;

        if force_full_sync {
            tracing::info!("Triggering full re-sync (24h periodic or initial sync)");
        } else {
            tracing::debug!("Running incremental readings sync...");
        }

        let mut retries = 0;
        let mut sync_succeeded = false;

        loop {
            match vaisala_sync::sync_readings(
                &state.db,
                vaisala_client,
                max_history_days,
                force_full_sync,
            )
            .await
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
                    if e.to_string().contains("Rate limited") && retries <= max_retries {
                        tracing::warn!(
                            retry = retries,
                            max_retries,
                            delay_secs = retry_delay_secs,
                            "Readings sync rate limited, retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(retry_delay_secs)).await;
                    } else if retries <= max_retries {
                        tracing::error!(
                            error = %e,
                            retry = retries,
                            max_retries,
                            "Readings sync failed, retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(retry_delay_secs)).await;
                    } else {
                        tracing::error!(
                            error = %e,
                            max_retries,
                            "Readings sync failed after max retries"
                        );
                        break;
                    }
                }
            }
        }

        if force_full_sync && sync_succeeded {
            state::update_last_full_sync_for_all_parameters(&state.db).await;
            state::refresh_continuous_aggregates_full(&state.db).await;
        } else if sync_succeeded {
            state::refresh_continuous_aggregates(&state.db).await;
        }

        ticker.tick().await;
    }
}
