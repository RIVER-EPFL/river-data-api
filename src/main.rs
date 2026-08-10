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

    // Worker-pool jobs left mid-flight by a dead process are recovered by the lease reaper (they
    // carry a lease). In-process jobs do not carry a lease, so reap this replica's own leaseless
    // orphans on boot — before any in-process job of this incarnation is spawned.
    match river_db::routes::private::reprocessing_jobs::lifecycle::reconcile_orphaned_inline_jobs(&db)
        .await
    {
        Ok(0) => {}
        Ok(n) => tracing::info!(reclaimed = n, "Reaped orphaned in-process jobs on startup"),
        Err(e) => tracing::warn!(error = %e, "Startup orphaned-job reconcile failed"),
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

    river_db::routes::private::sensors::calibrations::service::set_job_retry_policy(
        river_db::routes::private::sensors::calibrations::service::RetryPolicy {
            max_retries: config.job_max_retries,
            backoff_base: Duration::from_secs(config.job_retry_backoff_seconds),
        },
    );

    // The Telegram bot long-polls a global feed (not a competing-consumer queue), so it stays a
    // dedicated single-replica task — it is NOT a scheduled Service.
    if state.config.telegram_bot_token.is_some() {
        if state.config.enable_telegram_bot {
            tokio::spawn(river_db::routes::private::notifications::bot::run(state.clone()));
            tracing::info!("Spawned Telegram bot poller");
        } else {
            tracing::info!("Telegram bot poller disabled on this replica (ENABLE_TELEGRAM_BOT=false)");
        }
    }

    // The five former background loops (derived janitor, alarm sweeper, notification dispatcher,
    // identity reconciliation, channel health) are now recurring Services run through the DB-backed
    // scheduler: each fires as a Job claimed by exactly one replica per scheduled tick, so they no
    // longer double-fire at 2-3 k8s replicas. The registry carries their cadence from Config; the
    // scheduler seeds a `schedules` row per Service on first start and ticks them thereafter.
    let mut registry = river_db::routes::private::reprocessing_jobs::job::build_registry();
    river_db::routes::private::reprocessing_jobs::job::register_scheduled_services(
        &mut registry,
        &config,
    );
    let registry = std::sync::Arc::new(registry);

    if let Err(e) = river_db::routes::private::reprocessing_jobs::scheduler::seed_default_schedules(
        &db, &registry,
    )
    .await
    {
        tracing::error!(error = %e, "Failed to seed default schedules");
    } else {
        tracing::info!("Seeded recurring-service schedules");
    }

    tokio::spawn({
        let db = db.clone();
        let events = state.events.clone();
        let registry = registry.clone();
        let mut shutdown_rx = shutdown_rx.clone();
        async move {
            river_db::routes::private::reprocessing_jobs::worker::run(db, events, registry, async move {
                let _ = shutdown_rx.changed().await;
            })
            .await;
        }
    });
    tracing::info!("Spawned job worker");

    tokio::spawn({
        let db = db.clone();
        let registry = registry.clone();
        let mut shutdown_rx = shutdown_rx.clone();
        async move {
            river_db::routes::private::reprocessing_jobs::scheduler::run(db, registry, async move {
                let _ = shutdown_rx.changed().await;
            })
            .await;
        }
    });
    tracing::info!("Spawned recurring-service scheduler");

    // Preserve the low-latency reaction the dispatcher and sweeper had to live alarm-state changes:
    // the scheduled cadence is only the fallback. On every `AlarmStateChanged` broadcast (raised by
    // event-driven reconciles on ingest / config change / job completion), immediately enqueue an
    // alarm sweep and a notification drain. Both enqueues are dedupe-keyed on a coarse time bucket so
    // a burst of broadcasts collapses to at most one queued job of each kind, and `enqueue` is
    // `ON CONFLICT DO NOTHING` so exactly one replica wins.
    tokio::spawn({
        let db = db.clone();
        let mut rx = state.events.subscribe();
        let mut shutdown_rx = shutdown_rx.clone();
        async move {
            use river_db::common::AppEvent;
            use tokio::sync::broadcast::error::RecvError;
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx.changed() => return,
                    ev = rx.recv() => match ev {
                        Ok(AppEvent::AlarmStateChanged { .. }) => {
                            // 5s bucket: collapse a flurry of broadcasts into one immediate run while
                            // still firing promptly. The scheduled tick is the ongoing backstop.
                            let bucket = chrono::Utc::now().timestamp() / 5;
                            for job in ["alarm_sweep", "dispatch_notifications"] {
                                let key = format!("{job}:wake:{bucket}");
                                if let Err(e) = river_db::routes::private::reprocessing_jobs::worker::enqueue(
                                    &db, job, None, None, &serde_json::json!({ "trigger": "alarm_state_changed" }), Some(&key),
                                ).await {
                                    tracing::warn!(error = %e, job, "failed to enqueue on alarm-state broadcast");
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => return,
                    },
                }
            }
        }
    });
    tracing::info!("Spawned alarm-state broadcast bridge");

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
