use sea_orm::Database;
use sea_orm_migration::MigratorTrait;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use axum_keycloak_auth::instance::{KeycloakAuthInstance, KeycloakConfig};
use axum_keycloak_auth::Url;

use river_db::common::AppState;
use river_db::config::{Config, Deployment};
use river_db::connectors::vaisala::{self, VaisalaClient};
use river_db::routes;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing (sqlx::query disabled to reduce log noise)
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,river_db=debug,sqlx::query=warn".into()),
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

    // Create Vaisala client (optional - sync disabled if not configured)
    let vaisala_client = config.vaisala.as_ref().map(VaisalaClient::new);
    if vaisala_client.is_some() {
        tracing::info!("Vaisala client initialized");
    } else {
        tracing::info!("Vaisala not configured, sync tasks will be skipped");
    }

    // Initialize Keycloak authentication (optional in dev, required in prod)
    let keycloak_instance = match (&config.keycloak_url, &config.keycloak_realm) {
        (Some(url), Some(realm)) => {
            tracing::info!(url = %url, realm = %realm, "Initializing Keycloak authentication");
            Some(Arc::new(KeycloakAuthInstance::new(
                KeycloakConfig::builder()
                    .server(Url::parse(url).expect("Invalid KEYCLOAK_URL"))
                    .realm(realm.clone())
                    .build(),
            )))
        }
        _ => {
            if matches!(config.deployment, Deployment::Prod) {
                panic!(
                    "SECURITY ERROR: Keycloak authentication is required in production. \
                     Configure KEYCLOAK_URL and KEYCLOAK_REALM environment variables."
                );
            }
            tracing::warn!("Keycloak authentication NOT configured — admin routes unprotected");
            None
        }
    };

    // Create application state
    let state = AppState::new(db, config.clone(), vaisala_client, keycloak_instance);

    // Spawn background sync tasks (fire-and-forget, non-blocking)
    if state.vaisala_client.is_some() {
        tracing::info!("Spawning background sync tasks...");
        tokio::spawn(vaisala::scheduler::run_readings_sync(state.clone()));
    } else {
        tracing::info!("Vaisala not configured, skipping sync tasks");
    }

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
