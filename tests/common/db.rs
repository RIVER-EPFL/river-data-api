use std::time::{Duration, Instant};

use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;

/// The advisory-lock key one suite holds for its whole run. Advisory locks are per-database, so a
/// run against an isolated database never waits on the shared one.
pub const HARNESS_LOCK_KEY: i64 = 0x5249_5645_52; // "RIVER"

/// How long a second runner waits for the first to finish before giving up and saying so.
const LOCK_WAIT: Duration = Duration::from_secs(1800);

/// The session holding [`HARNESS_LOCK_KEY`], kept for the life of the process. Every test builds
/// its own runtime, so this connection outlives the one that opened it; it is never queried again,
/// and Postgres releases the lock when the process exits.
static LOCK_SESSION: tokio::sync::OnceCell<DatabaseConnection> = tokio::sync::OnceCell::const_new();

/// Take the database for this process, waiting for whoever holds it.
///
/// `cleanup_test_db` truncates tables a concurrent suite is reading, so two runners on one database
/// produce failures that read like product defects. Waiting is the only honest answer: the second
/// runner is not wrong, it is early.
async fn hold_the_database(url: &str) {
    LOCK_SESSION
        .get_or_init(|| async {
            let mut opts = ConnectOptions::new(url.to_string());
            opts.max_connections(1).min_connections(1).sqlx_logging(false);
            let session = Database::connect(opts)
                .await
                .expect("Failed to open the harness lock session");

            let deadline = Instant::now() + LOCK_WAIT;
            loop {
                let row = session
                    .query_one_raw(Statement::from_string(
                        sea_orm::DatabaseBackend::Postgres,
                        format!("SELECT pg_try_advisory_lock({HARNESS_LOCK_KEY}) AS taken"),
                    ))
                    .await
                    .expect("harness lock query")
                    .expect("harness lock row");
                if row.try_get::<bool>("", "taken").expect("taken") {
                    return session;
                }
                assert!(
                    Instant::now() < deadline,
                    "another suite has held {url} for {}s. Stop the test watcher, or run against \
                     an isolated database.",
                    LOCK_WAIT.as_secs()
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
        .await;
}

pub async fn setup_test_db() -> DatabaseConnection {
    dotenvy::dotenv().ok();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    hold_the_database(&url).await;

    // The deployment serves handlers from a 25-connection pool and the job worker from its own.
    // The harness runs both plus the test's own queries on one pool, so it takes the wider ceiling:
    // sqlx defaults to ten, which a job holding connections across tool runs can exhaust.
    let mut opts = ConnectOptions::new(url);
    opts.max_connections(crate::common::test_config().db_max_connections)
        .min_connections(1)
        .acquire_timeout(Duration::from_secs(30))
        .sqlx_logging(false);
    let db = Database::connect(opts)
        .await
        .expect("Failed to connect to test database");

    migration::Migrator::up(&db, None)
        .await
        .expect("Failed to run migrations");

    db
}

pub async fn cleanup_test_db(db: &DatabaseConnection) {
    let stmts = [
        "SELECT remove_continuous_aggregate_policy('readings_monthly', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_weekly', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_daily', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_hourly', if_not_exists => true)",
        // The four rollups are materialized hypertables of their own, so truncating `readings`
        // leaves their buckets in place and the next test reading extents from a summary sees the
        // previous test's data.
        "TRUNCATE readings_hourly, readings_daily, readings_weekly, readings_monthly",
        // `constants` is migration-seeded reference data; truncating it cannot be undone by
        // seed_test_data, so it is deliberately absent from this list.
        "TRUNCATE readings, reading_decisions, status_events, samples, \
         tool_runs, seasonal_checks, collection_events, \
         sync_service_tokens, sync_events, sync_commands, sync_services, sync_service_credentials, \
         pairing_plans, data_streams, \
         reprocessing_jobs, schedules, schedule_audit, csv_import_staging, \
         annotations, notes, \
         alarm_thresholds, alarm_events, api_tokens, \
         web_push_subscriptions, \
         notification_mutes, notification_log, notification_state, \
         notification_subscribers, notification_subscriptions, notification_channel_health, \
         sensor_calibrations, sensor_deployments, sensors, \
         derived_parameter_sources, derived_parameter_definitions, \
         user_project_grants, \
         site_parameters, parameters, sites, subprojects, projects CASCADE",
    ];

    for sql in &stmts {
        let _ = db
            .execute_raw(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                sql.to_string(),
            ))
            .await;
    }
}

pub async fn exec(db: &DatabaseConnection, sql: &str) {
    db.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("SQL failed: {e}\nQuery: {sql}"));
}
