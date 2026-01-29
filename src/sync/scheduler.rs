use std::time::Duration;
use tokio::time::interval;

use crate::common::AppState;
use crate::sync::worker;

/// Run the readings sync task on a schedule.
///
/// On startup, first discovers locations (zones/stations/sensors) from Vaisala,
/// then performs incremental syncs every interval, with a full re-sync every 24 hours.
pub async fn run_readings_sync(state: AppState) {
    let interval_secs = state.config.sync_readings_interval_seconds;
    let max_history_days = state.config.vaisala_max_history_days;
    let retry_delay_secs = state.config.sync_retry_delay_seconds;
    let max_retries = state.config.sync_retry_max;

    tracing::info!(
        interval_secs,
        max_history_days,
        "Starting readings sync scheduler"
    );

    // Discover locations from Vaisala on startup
    if let Err(e) = worker::sync_locations(&state.db, &state.vaisala_client).await {
        tracing::error!(error = %e, "Failed to discover locations from Vaisala");
    }

    let mut ticker = interval(Duration::from_secs(interval_secs));

    // Run initial sync immediately
    ticker.tick().await;

    loop {
        // Check if we need a full re-sync (every 24 hours)
        let force_full_sync = worker::needs_full_sync(&state.db).await;

        if force_full_sync {
            tracing::info!("Triggering full re-sync (24h periodic or initial sync)");
        } else {
            tracing::debug!("Running incremental readings sync...");
        }

        let mut retries = 0;
        let mut sync_succeeded = false;

        loop {
            match worker::sync_readings(
                &state.db,
                &state.vaisala_client,
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

        // If full sync succeeded, update the last_full_sync timestamp for all sensors
        // and refresh aggregates for the entire history
        if force_full_sync && sync_succeeded {
            worker::update_last_full_sync_for_all_sensors(&state.db).await;
            worker::refresh_continuous_aggregates_full(&state.db).await;
        } else if sync_succeeded {
            // Incremental sync: only refresh recent data
            worker::refresh_continuous_aggregates(&state.db).await;
        }

        // Wait for next tick
        ticker.tick().await;
    }
}

/// Run the device status sync task on a schedule.
pub async fn run_device_status_sync(state: AppState) {
    let interval_secs = state.config.sync_device_status_interval_seconds;
    let retry_delay_secs = state.config.sync_retry_delay_seconds;
    let max_retries = state.config.sync_retry_max;

    tracing::info!(interval_secs, "Starting device status sync scheduler");

    let mut ticker = interval(Duration::from_secs(interval_secs));

    // Run initial sync immediately
    ticker.tick().await;

    loop {
        tracing::debug!("Running device status sync...");

        let mut retries = 0;
        loop {
            match worker::sync_device_status(&state.db, &state.vaisala_client).await {
                Ok(()) => {
                    tracing::debug!("Device status sync completed successfully");
                    break;
                }
                Err(e) => {
                    retries += 1;
                    if e.to_string().contains("Rate limited") && retries <= max_retries {
                        tracing::warn!(
                            retry = retries,
                            max_retries,
                            delay_secs = retry_delay_secs,
                            "Device status sync rate limited, retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(retry_delay_secs)).await;
                    } else if retries <= max_retries {
                        tracing::error!(
                            error = %e,
                            retry = retries,
                            max_retries,
                            "Device status sync failed, retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(retry_delay_secs)).await;
                    } else {
                        tracing::error!(
                            error = %e,
                            max_retries,
                            "Device status sync failed after max retries"
                        );
                        break;
                    }
                }
            }
        }

        // Wait for next tick
        ticker.tick().await;
    }
}

/// Run the alarms sync task on a schedule.
pub async fn run_alarms_sync(state: AppState) {
    let interval_secs = state.config.sync_alarms_interval_seconds;
    let retry_delay_secs = state.config.sync_retry_delay_seconds;
    let max_retries = state.config.sync_retry_max;

    tracing::info!(interval_secs, "Starting alarms sync scheduler");

    let mut ticker = interval(Duration::from_secs(interval_secs));

    // Run initial sync immediately
    ticker.tick().await;

    loop {
        tracing::debug!("Running alarms sync...");

        let mut retries = 0;
        loop {
            match worker::sync_alarms(&state.db, &state.vaisala_client).await {
                Ok(()) => {
                    tracing::debug!("Alarms sync completed successfully");
                    break;
                }
                Err(e) => {
                    retries += 1;
                    if e.to_string().contains("Rate limited") && retries <= max_retries {
                        tracing::warn!(
                            retry = retries,
                            max_retries,
                            delay_secs = retry_delay_secs,
                            "Alarms sync rate limited, retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(retry_delay_secs)).await;
                    } else if retries <= max_retries {
                        tracing::error!(
                            error = %e,
                            retry = retries,
                            max_retries,
                            "Alarms sync failed, retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(retry_delay_secs)).await;
                    } else {
                        tracing::error!(
                            error = %e,
                            max_retries,
                            "Alarms sync failed after max retries"
                        );
                        break;
                    }
                }
            }
        }

        // Wait for next tick
        ticker.tick().await;
    }
}

/// Run the events sync task on a schedule.
pub async fn run_events_sync(state: AppState) {
    let interval_secs = state.config.sync_events_interval_seconds;
    let retry_delay_secs = state.config.sync_retry_delay_seconds;
    let max_retries = state.config.sync_retry_max;

    tracing::info!(interval_secs, "Starting events sync scheduler");

    let mut ticker = interval(Duration::from_secs(interval_secs));

    // Run initial sync immediately
    ticker.tick().await;

    loop {
        tracing::debug!("Running events sync...");

        let mut retries = 0;
        loop {
            match worker::sync_events(&state.db, &state.vaisala_client).await {
                Ok(()) => {
                    tracing::debug!("Events sync completed successfully");
                    break;
                }
                Err(e) => {
                    retries += 1;
                    if e.to_string().contains("Rate limited") && retries <= max_retries {
                        tracing::warn!(
                            retry = retries,
                            max_retries,
                            delay_secs = retry_delay_secs,
                            "Events sync rate limited, retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(retry_delay_secs)).await;
                    } else if retries <= max_retries {
                        tracing::error!(
                            error = %e,
                            retry = retries,
                            max_retries,
                            "Events sync failed, retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(retry_delay_secs)).await;
                    } else {
                        tracing::error!(
                            error = %e,
                            max_retries,
                            "Events sync failed after max retries"
                        );
                        break;
                    }
                }
            }
        }

        // Wait for next tick
        ticker.tick().await;
    }
}
