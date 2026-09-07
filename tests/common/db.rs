use std::time::{Duration, Instant};

use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;

/// The advisory-lock key one suite holds for its whole run. Advisory locks are per-database, so a
/// run against an isolated database never waits on the shared one.
pub const HARNESS_LOCK_KEY: i64 = 0x5249_5645_52; // "RIVER"

/// How long a second runner waits for the first to finish before giving up and saying so.
const LOCK_WAIT: Duration = Duration::from_secs(1800);

/// How many suites the server must hold at once. The advisory lock isolates a database, not the
/// server: the audit's own rules give every agent its own database on this one instance, so
/// several suites hold their pools at the same time.
const CONCURRENT_SUITES: i64 = 4;

/// Refuse a server that cannot hold the fleet's pools.
///
/// Postgres defaults to 100 connections. Four suites at [`test_config`]'s pool size exceed that,
/// and what a run over the ceiling looks like is not a connection error but a wall of assertion
/// failures in whichever tests happened to need a connection, each of which passes alone. Asked
/// here, the answer is one line before the first test.
///
/// [`test_config`]: crate::common::test_config
async fn require_connection_headroom(db: &DatabaseConnection, url: &str) {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT current_setting('max_connections')::bigint \
                  - current_setting('superuser_reserved_connections')::bigint AS ceiling"
                .to_string(),
        ))
        .await
        .expect("read max_connections")
        .expect("max_connections row");
    let ceiling: i64 = row.try_get("", "ceiling").expect("ceiling");
    let pool = i64::from(crate::common::test_config().db_max_connections);
    let needed = pool * CONCURRENT_SUITES;
    assert!(
        ceiling >= needed,
        "{url} leaves {ceiling} connections, and {CONCURRENT_SUITES} suites at {pool} each need \
         {needed}. Raise max_connections on the test database service."
    );
}

/// Stop TimescaleDB's own background policies on this database.
///
/// The baseline installs a compression policy on `readings` and on `status_events`. Its scheduler
/// runs them inside a test's window, taking chunk locks while `cleanup_test_db` is taking
/// ACCESS EXCLUSIVE over the same table, and the pair deadlocks: one test per run fails with a
/// `deadlock detected` raised out of a cleanup statement, passes alone and passes on the rerun.
/// Nothing in the suite depends on the policies firing, so the harness removes them; the tests
/// that need a compressed chunk compress it themselves (`tests/common/compression.rs`).
async fn stop_background_policies(db: &DatabaseConnection) {
    let jobs = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT job_id FROM timescaledb_information.jobs \
              WHERE proc_name = 'policy_compression'"
                .to_string(),
        ))
        .await
        .expect("list the background policies");
    for row in jobs {
        let job_id: i32 = row.try_get("", "job_id").expect("job_id");
        db.execute_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT delete_job({job_id})"),
        ))
        .await
        .expect("delete the background policy");
    }
}

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
    let url_for_message = url.clone();
    let mut opts = ConnectOptions::new(url);
    opts.max_connections(crate::common::test_config().db_max_connections)
        .min_connections(1)
        .acquire_timeout(Duration::from_secs(30))
        .sqlx_logging(false);
    let db = Database::connect(opts)
        .await
        .expect("Failed to connect to test database");

    require_connection_headroom(&db, &url_for_message).await;

    migration::Migrator::up(&db, None)
        .await
        .expect("Failed to run migrations");

    stop_background_policies(&db).await;

    // Every channel carries an instrument in the deployed system: registration mints one, and a
    // reading may not be stored without naming what measured it. A fixture stream built by raw SQL
    // has no source to mint from, so the suite gives it [`FIXTURE_SENSOR_ID`] as the column
    // default. A stream created through a route still sets the column itself, so the minting paths
    // are exercised, not defaulted around.
    db.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("ALTER TABLE data_streams ALTER COLUMN sensor_id SET DEFAULT '{FIXTURE_SENSOR_ID}'"),
    ))
    .await
    .expect("default the fixture instrument onto hand-built streams");

    db
}

/// The instrument a hand-built fixture stream belongs to.
pub const FIXTURE_SENSOR_ID: &str = "00000000-0000-4000-e000-000000000999";

/// The tables the TRUNCATE empties. With [`CLEANUP_DELETED_TABLES`] this is the whole of what
/// `cleanup_test_db` covers, and `tests/harness/cleanup_coverage.rs` holds the pair against the
/// schema.
///
/// `constants` is migration-seeded reference data; truncating it cannot be undone by
/// `seed_test_data`, so it is deliberately absent.
pub const CLEANUP_TRUNCATED_TABLES: &[&str] = &[
    "readings",
    "reading_decisions",
    "reading_decision_sets",
    "status_events",
    "samples",
    "tool_runs",
    "seasonal_checks",
    "collection_events",
    "sync_service_tokens",
    "sync_events",
    "sync_commands",
    "sync_services",
    "sync_service_credentials",
    "pairing_plans",
    "data_streams",
    "reprocessing_jobs",
    "schedules",
    "change_audit",
    "csv_import_staging",
    "annotations",
    "notes",
    "alarm_thresholds",
    "alarm_events",
    "api_tokens",
    "api_token_audit_log",
    "web_push_subscriptions",
    "notification_mutes",
    "notification_log",
    "notification_state",
    "notification_subscribers",
    "notification_subscriptions",
    "sensor_calibrations",
    "sensor_deployments",
    "sensors",
    "derived_parameter_sources",
    "derived_parameter_definitions",
    "user_project_grants",
    "site_parameters",
    "parameters",
    "sites",
    "subprojects",
    "projects",
];

/// The tables emptied by statement instead, because their migration-seeded rows stay.
pub const CLEANUP_DELETED_TABLES: &[&str] = &[
    "parameter_group_members",
    "parameter_groups",
    "tool_script_activations",
    "tool_script_versions",
    "tool_scripts",
];

/// Reference data a test reads and never owns.
pub const CLEANUP_EXEMPT_TABLES: &[&str] = &["constants", "seaql_migrations"];

pub async fn cleanup_test_db(db: &DatabaseConnection) {
    // The fixture's own writer goes first: a worker still claiming rows while this truncates is
    // the one thing in the harness that can write between the TRUNCATE and the seed.
    crate::common::stop_test_workers().await;
    let stmts = [
        "SELECT remove_continuous_aggregate_policy('readings_monthly', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_weekly', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_daily', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_hourly', if_not_exists => true)",
        // The four rollups are materialized hypertables of their own, so truncating `readings`
        // leaves their buckets in place and the next test reading extents from a summary sees the
        // previous test's data.
        "TRUNCATE readings_hourly, readings_daily, readings_weekly, readings_monthly",
        // Tool scripts a test installed. The migration-seeded ones (`created_by = 'seed'`) are
        // reference data and stay; a fixture left enabled makes the next theme's visits recompute
        // against a calculation that theme never installed.
        "DELETE FROM tool_script_activations a USING tool_scripts s \
          WHERE a.tool_script_id = s.id AND s.created_by IS DISTINCT FROM 'seed'",
        "UPDATE tool_scripts SET active_version_id = NULL WHERE created_by IS DISTINCT FROM 'seed'",
        "DELETE FROM tool_script_versions v USING tool_scripts s \
          WHERE v.tool_script_id = s.id AND s.created_by IS DISTINCT FROM 'seed'",
        // A formula calculation owns `derived_parameter_definitions` rows, which cascade with the
        // calculation; what hangs off them does not, so the sources and the slots naming a
        // definition go first or the delete below is refused by their foreign keys.
        "DELETE FROM derived_parameter_sources src \
           USING derived_parameter_definitions d JOIN tool_scripts s ON s.id = d.tool_script_id \
           WHERE src.derived_definition_id = d.id AND s.created_by IS DISTINCT FROM 'seed'",
        "UPDATE site_parameters sp SET derived_definition_id = NULL \
           FROM derived_parameter_definitions d JOIN tool_scripts s ON s.id = d.tool_script_id \
          WHERE sp.derived_definition_id = d.id AND s.created_by IS DISTINCT FROM 'seed'",
        "DELETE FROM tool_scripts WHERE created_by IS DISTINCT FROM 'seed'",
        // Deleted rather than truncated: `tool_scripts.parameter_group_id` references
        // `parameter_groups`, so a TRUNCATE ... CASCADE over the groups would take the seeded
        // calculations with it.
        "DELETE FROM parameter_group_members",
        "UPDATE tool_scripts SET parameter_group_id = NULL",
        "DELETE FROM parameter_groups",
    ];

    for sql in &stmts {
        exec(db, sql).await;
    }
    exec(
        db,
        &format!("TRUNCATE {} CASCADE", CLEANUP_TRUNCATED_TABLES.join(", ")),
    )
    .await;

    // Truncated with the rest, and restored here: the column default above points at it.
    exec(
        db,
        &format!(
            "INSERT INTO sensors (id, name, is_active, source_system, source_key) \
             VALUES ('{FIXTURE_SENSOR_ID}', 'Fixture instrument', true, 'fixture', 'fixture') \
             ON CONFLICT DO NOTHING"
        ),
    )
    .await;
}

/// Run SQL through the simple protocol, for a script of several statements.
pub async fn exec_unprepared(db: &DatabaseConnection, sql: &str) {
    db.execute_unprepared(sql)
        .await
        .unwrap_or_else(|e| panic!("SQL failed: {e}\nQuery: {sql}"));
}

pub async fn exec(db: &DatabaseConnection, sql: &str) {
    db.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("SQL failed: {e}\nQuery: {sql}"));
}

/// Seed rows that predate the instrument rule.
///
/// `readings_instrument_required` refuses a reading that names no instrument, so a fixture for the
/// paths that exist to repair such rows (the import backfill, the attribution candidates) cannot
/// insert one while the rule stands. The rule is lifted for the statements and restored with its
/// own definition afterwards, which also asserts that what was seeded is legal again by the time
/// the test looks at it.
pub async fn seed_before_instrument_rule(db: &DatabaseConnection, statements: &[String]) {
    // Both halves of the rule stand aside: the trigger would fill `sensor_id` from the row's
    // stream, and the CHECK would then be moot anyway. The trigger is dropped and recreated rather
    // than disabled, which a hypertable with columnstore refuses; its function is untouched.
    exec(
        db,
        "DROP TRIGGER IF EXISTS trg_readings_inherit_stream_instrument ON readings",
    )
    .await;
    exec(
        db,
        "ALTER TABLE readings DROP CONSTRAINT IF EXISTS readings_instrument_required",
    )
    .await;
    for sql in statements {
        exec(db, sql).await;
    }
    exec(
        db,
        "CREATE TRIGGER trg_readings_inherit_stream_instrument \
         BEFORE INSERT OR UPDATE ON readings \
         FOR EACH ROW EXECUTE FUNCTION readings_inherit_stream_instrument()",
    )
    .await;
    // `NOT VALID` puts the rule back over everything written from here without validating the rows
    // just seeded, which is what a database carried forward from before the rule looks like.
    exec(
        db,
        "ALTER TABLE readings ADD CONSTRAINT readings_instrument_required \
         CHECK ((sensor_id IS NOT NULL) OR (measurement_type IS NOT DISTINCT FROM 'derived')) \
         NOT VALID",
    )
    .await;
}

/// Assert the rows [`seed_before_instrument_rule`] left behind now satisfy the instrument rule,
/// which is how a test says the repair it drove missed nothing. The constraint itself cannot be
/// re-validated: `VALIDATE CONSTRAINT` is refused on a hypertable with columnstore enabled, so the
/// same predicate is asked as a query.
pub async fn assert_instrument_rule_holds(db: &DatabaseConnection) {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*) AS n FROM readings \
             WHERE sensor_id IS NULL AND measurement_type IS DISTINCT FROM 'derived'"
                .to_string(),
        ))
        .await
        .expect("query")
        .expect("count row");
    let unattributed: i64 = row.try_get("", "n").expect("count");
    assert_eq!(
        unattributed, 0,
        "every reading names an instrument, or is derived"
    );
}
