use sea_orm::{ConnectOptions, Database};
use sea_orm_migration::MigratorTrait;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::signal;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use axum_keycloak_auth::Url;
use axum_keycloak_auth::instance::{KeycloakAuthInstance, KeycloakConfig};

use river_db::common::AppState;
use river_db::config::{Config, Deployment};
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

    // Connect to database with pool tuning (fail-fast)
    tracing::info!("Connecting to database...");
    let mut db_opts = ConnectOptions::new(&config.database_url);
    db_opts
        .max_connections(config.db_max_connections)
        .min_connections(config.db_min_connections)
        .connect_timeout(Duration::from_secs(5))
        .idle_timeout(Duration::from_secs(300))
        .sqlx_logging(false)
        .set_schema_search_path("public");
    let db = Database::connect(db_opts).await?;
    tracing::info!(
        max_connections = config.db_max_connections,
        min_connections = config.db_min_connections,
        "Database connection pool established"
    );

    // Set statement timeout as defense-in-depth against runaway queries
    use sea_orm::ConnectionTrait;
    db.execute(sea_orm::Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SET statement_timeout = '30s'".to_string(),
    ))
    .await?;
    tracing::info!("Statement timeout set to 30s");

    // Run migrations under a cross-replica advisory lock. With 2-3 replicas booting together,
    // concurrent Migrator::up can deadlock on ALTER / non-CONCURRENT CREATE INDEX. A dedicated
    // single-connection handle holds a session lock for the whole run — lock and unlock land on the
    // same connection so it can't leak — serialising migrations across replicas; once one replica has
    // applied them the others no-op. (Timescale hypertable/CAGG DDL can't run inside one txn, so a
    // transaction-scoped lock isn't an option.)
    tracing::info!("Running migrations...");
    let mut lock_opts = ConnectOptions::new(&config.database_url);
    lock_opts
        .max_connections(1)
        .min_connections(1)
        .connect_timeout(Duration::from_secs(5))
        .sqlx_logging(false);
    let lock_db = Database::connect(lock_opts).await?;
    lock_db
        .execute_unprepared("SELECT pg_advisory_lock(8526340921)")
        .await?;
    let migrate_result = migration::Migrator::up(&db, None).await;
    let _ = lock_db
        .execute_unprepared("SELECT pg_advisory_unlock(8526340921)")
        .await;
    lock_db.close().await?;
    migrate_result?;
    tracing::info!("Migrations completed");

    // Reconcile any tracked jobs left mid-flight by a previous process (their background tasks died
    // with that process). Must run after migrations and before the janitor spawn / HTTP server, so
    // it can't sweep a job that legitimately just started.
    match river_db::routes::private::reprocessing_jobs::lifecycle::reconcile_interrupted_jobs(&db)
        .await
    {
        Ok(0) => {}
        Ok(n) => tracing::warn!(count = n, "Marked orphaned reprocessing jobs as interrupted"),
        Err(e) => tracing::error!(error = %e, "Failed to reconcile interrupted reprocessing jobs"),
    }

    // Initialize Keycloak authentication (optional in dev, required in prod)
    let keycloak_instance = if let (Some(url), Some(realm)) = (&config.keycloak_url, &config.keycloak_realm) {
        tracing::info!(url = %url, realm = %realm, "Initializing Keycloak authentication");
        Some(Arc::new(KeycloakAuthInstance::new(
            KeycloakConfig::builder()
                .server(Url::parse(url).expect("Invalid KEYCLOAK_URL"))
                .realm(realm.clone())
                .build(),
        )))
    } else {
        if matches!(config.deployment, Deployment::Prod) {
            panic!(
                "SECURITY ERROR: Keycloak authentication is required in production. \
                 Configure KEYCLOAK_URL and KEYCLOAK_REALM environment variables."
            );
        }
        tracing::warn!("Keycloak authentication NOT configured — admin routes unprotected");
        None
    };

    let state = AppState::new(db.clone(), config.clone(), keycloak_instance);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    river_db::routes::private::sensor_calibrations::services::set_job_retry_policy(
        river_db::routes::private::sensor_calibrations::services::RetryPolicy {
            max_retries: config.job_max_retries,
            backoff_base: Duration::from_secs(config.job_retry_backoff_seconds),
        },
    );

    let janitor_interval = Duration::from_secs(config.janitor_interval_seconds);
    let janitor_full_refresh = Duration::from_secs(config.janitor_full_refresh_seconds);
    tokio::spawn(river_db::routes::private::derived_parameters::janitor::periodic(
        db.clone(),
        janitor_interval,
        janitor_full_refresh,
        config.job_maintenance_retention_days,
        config.janitor_retention_days,
        config.job_maintenance_max_rows,
    ));
    tracing::info!(
        interval_secs = janitor_interval.as_secs(),
        full_refresh_secs = janitor_full_refresh.as_secs(),
        maintenance_retention_days = config.job_maintenance_retention_days,
        operator_retention_days = config.janitor_retention_days,
        "Spawned derived consistency janitor"
    );

    let alarm_sweep_interval = Duration::from_secs(config.alarm_sweep_interval_seconds);
    tokio::spawn(river_db::routes::private::alarms::sweeper::periodic(
        db.clone(),
        alarm_sweep_interval,
        state.events.clone(),
    ));
    tracing::info!(
        interval_secs = alarm_sweep_interval.as_secs(),
        "Spawned alarm sweeper"
    );

    tokio::spawn(river_db::routes::private::notifications::dispatcher::periodic(
        db.clone(),
        state.config.clone(),
        state.events.clone(),
    ));
    tracing::info!("Spawned notification dispatcher");

    if state.config.telegram_bot_token.is_some() {
        tokio::spawn(river_db::routes::private::notifications::bot::run(state.clone()));
        tracing::info!("Spawned Telegram bot poller");
    }

    let reconcile_interval = Duration::from_secs(config.identity_reconcile_interval_seconds);
    tokio::spawn(river_db::routes::private::notifications::reconcile::periodic(
        state.clone(),
        reconcile_interval,
    ));
    tracing::info!(
        interval_secs = reconcile_interval.as_secs(),
        "Spawned Telegram identity reconciliation"
    );

    tokio::spawn(river_db::routes::private::notifications::health::periodic(
        db.clone(),
        state.config.clone(),
    ));
    tracing::info!("Spawned notification channel health probe");

    let registry = std::sync::Arc::new(
        river_db::routes::private::reprocessing_jobs::job::build_registry(),
    );
    tokio::spawn({
        let db = db.clone();
        let events = state.events.clone();
        let mut shutdown_rx = shutdown_rx.clone();
        async move {
            river_db::routes::private::reprocessing_jobs::worker::run(db, events, registry, async move {
                let _ = shutdown_rx.changed().await;
            })
            .await;
        }
    });
    tracing::info!("Spawned job worker");

    let app = routes::build_router(state);

    // Start server with graceful shutdown
    let addr = config.bind_address();
    tracing::info!(address = %addr, "Starting server");
    let listener = TcpListener::bind(&addr).await?;
    let mut server_shutdown = shutdown_rx.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = server_shutdown.changed().await;
        })
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
