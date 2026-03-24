mod backend;
mod config;
mod db;
mod error;
mod service;
mod sync;

use river_data_sync_common::models::RunnerConfig;
use river_data_sync_common::river_data_client::RiverDataClient;
use river_data_sync_common::runner::SyncServiceRunner;

use crate::backend::nomis::NomisBackend;
use crate::backend::rshiny::RshinyBackend;
use crate::backend::PortalBackend;
use crate::config::{PortalConfig, PortalType};
use crate::service::PortalSyncService;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Load .env if present
    let _ = dotenvy::dotenv();

    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Load configuration
    let portal_config = PortalConfig::from_env()?;
    let runner_config = RunnerConfig::from_env().map_err(|e| e.to_string())?;

    tracing::info!(
        portal_type = portal_config.portal_type.source_system(),
        db_host = %portal_config.db_host,
        db_name = %portal_config.db_name,
        "Starting portal sync service"
    );

    // Connect to portal database
    let pool = db::create_pool(&portal_config).await?;

    // Create the appropriate backend
    let backend: Box<dyn PortalBackend> = match portal_config.portal_type {
        PortalType::Cnet => Box::new(RshinyBackend::new(PortalType::Cnet)),
        PortalType::Metalp => Box::new(RshinyBackend::new(PortalType::Metalp)),
        PortalType::Nomis => Box::new(NomisBackend::new()),
    };

    // Create river-data API client (token will be set after enrollment)
    let api = RiverDataClient::new(&runner_config.api_base_url, "");

    // Create and run the sync service
    let service = PortalSyncService::new(portal_config, api, pool, backend);
    let runner = SyncServiceRunner::new(service, runner_config);

    runner.run().await
}
