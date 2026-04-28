mod config;
mod models;
mod service;
mod sync;
mod vaisala_client;

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use config::SyncConfig;
use river_data_sync_common::models::RunnerConfig;
use river_data_sync_common::river_data_client::RiverDataClient;
use river_data_sync_common::runner::SyncServiceRunner;
use service::VaisalaSyncService;
use vaisala_client::VaisalaClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

    // Load Vaisala-specific configuration (fail-fast)
    let config = SyncConfig::from_env().map_err(|e| {
        tracing::error!(error = %e, "Configuration error");
        e
    })?;

    // Load runner configuration (control plane credentials)
    let runner_config = RunnerConfig::from_env().map_err(|e| {
        tracing::error!(error = %e, "Runner configuration error");
        e
    })?;

    tracing::info!(
        api_base_url = %config.api_base_url,
        vaisala_base_url = %config.vaisala_base_url,
        sync_interval_secs = runner_config.sync_interval_secs,
        instance_id = %runner_config.instance_id,
        "Configuration loaded"
    );

    // Create clients — token will be set by the runner after enrollment
    let api = RiverDataClient::new(&config.api_base_url, "")?;
    let vaisala = VaisalaClient::new(
        &config.vaisala_base_url,
        &config.vaisala_bearer_token,
        config.vaisala_skip_tls_verify,
    )?;

    // Create service and runner
    let svc = VaisalaSyncService::new(config, api, vaisala);
    let runner = SyncServiceRunner::new(svc, runner_config);

    // Run (blocks until shutdown signal)
    runner.run().await?;

    Ok(())
}
