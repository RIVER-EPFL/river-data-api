use sea_orm::Database;
use sea_orm_migration::MigratorTrait;
use tokio::net::TcpListener;
use tokio::signal;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use river_db::common::AppState;
use river_db::config::Config;
use river_db::routes;
use river_db::sync;
use river_db::vaisala::VaisalaClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,river_db=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!("Starting river-db...");

    // Load configuration (fail-fast)
    let config = Config::from_env()?;
    tracing::info!(
        deployment = ?config.deployment,
        host = %config.api_host,
        port = config.api_port,
        "Configuration loaded"
    );

    // Connect to database (fail-fast)
    tracing::info!("Connecting to database...");
    let db = Database::connect(&config.database_url).await?;
    tracing::info!("Database connection established");

    // Run migrations
    tracing::info!("Running migrations...");
    migration::Migrator::up(&db, None).await?;
    tracing::info!("Migrations completed");

    // Create Vaisala client
    let vaisala_client = VaisalaClient::new(&config);
    tracing::info!("Vaisala client initialized");

    // Create application state
    let state = AppState::new(db, config.clone(), vaisala_client);

    // Spawn background sync tasks (fire-and-forget, non-blocking)
    tracing::info!("Spawning background sync tasks...");
    tokio::spawn(sync::scheduler::run_readings_sync(state.clone()));
    tokio::spawn(sync::scheduler::run_device_status_sync(state.clone()));

    // Build router
    let app = routes::build_router(state);

    // Start server with graceful shutdown
    let addr = config.bind_address();
    tracing::info!(address = %addr, "Starting server");
    let listener = TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("Server shut down gracefully");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {
            tracing::info!("Received Ctrl+C, shutting down...");
        },
        () = terminate => {
            tracing::info!("Received SIGTERM, shutting down...");
        },
    }
}
