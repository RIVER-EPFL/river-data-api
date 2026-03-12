mod api_client;
mod config;
mod models;
mod scheduler;
mod sync;
mod vaisala_client;

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use api_client::ApiClient;
use config::SyncConfig;
use vaisala_client::VaisalaClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,river_data_sync_vaisala=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!("Starting river-data-sync-vaisala...");

    // Load .env if present
    let _ = dotenvy::dotenv();

    // Load configuration (fail-fast)
    let config = SyncConfig::from_env().map_err(|e| {
        tracing::error!(error = %e, "Configuration error");
        e
    })?;

    tracing::info!(
        api_base_url = %config.api_base_url,
        vaisala_base_url = %config.vaisala_base_url,
        sync_interval_secs = config.sync_interval_seconds,
        "Configuration loaded"
    );

    // Create clients
    let api = ApiClient::new(&config.api_base_url, &config.api_token);
    let vaisala = VaisalaClient::new(
        &config.vaisala_base_url,
        &config.vaisala_bearer_token,
        config.vaisala_skip_tls_verify,
    );

    // Spawn device status sync as independent background task
    let ds_config = config.clone();
    let ds_api = ApiClient::new(&ds_config.api_base_url, &ds_config.api_token);
    let ds_vaisala = VaisalaClient::new(
        &ds_config.vaisala_base_url,
        &ds_config.vaisala_bearer_token,
        ds_config.vaisala_skip_tls_verify,
    );
    tokio::spawn(async move {
        scheduler::run_device_status_sync(&ds_config, &ds_api, &ds_vaisala).await;
    });

    // Run the readings sync scheduler (blocks forever)
    scheduler::run_sync(&config, &api, &vaisala).await;

    Ok(())
}
