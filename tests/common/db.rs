use std::time::{Duration, Instant};

use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbErr, RuntimeErr, Statement,
};
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

/// What the harness lock session calls itself, so the cleanup sweep can spare it: terminating it
/// would drop [`HARNESS_LOCK_KEY`] and let a second runner in mid-suite.
const LOCK_SESSION_APPLICATION_NAME: &str = "river-harness-lock";

/// The lock session's URL, carrying the application name the sweep matches on.
fn lock_session_url(url: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}application_name={LOCK_SESSION_APPLICATION_NAME}")
}

/// The SQLSTATE Postgres answers a connection to a database that does not exist with.
const UNDEFINED_DATABASE: &str = "3D000";

/// The SQLSTATE a failed connection carries, when it carries one.
fn sqlstate(err: &DbErr) -> Option<String> {
    use std::ops::Deref;
    let DbErr::Conn(RuntimeErr::SqlxError(e)) = err else {
        return None;
    };
    let sea_orm::SqlxError::Database(e) = e.deref() else {
        return None;
    };
    e.code().map(|code| code.into_owned())
}

/// How many times a connection-level failure is tried again before the harness gives up.
const LOCK_SESSION_ATTEMPTS: u32 = 5;

/// How long the harness waits between those attempts.
const LOCK_SESSION_RETRY_DELAY: Duration = Duration::from_millis(250);

/// Whether a failed connect is worth trying again: a failure at the transport, not an answer from
/// the server. A reset on the first connect discards the whole binary's work, and the next connect
/// a moment later succeeds.
fn transient(err: &DbErr) -> bool {
    use std::ops::Deref;
    let DbErr::Conn(RuntimeErr::SqlxError(e)) = err else {
        return false;
    };
    matches!(
        e.deref(),
        sea_orm::SqlxError::Io(_) | sea_orm::SqlxError::Tls(_) | sea_orm::SqlxError::PoolTimedOut
    )
}

/// Connect, trying again on a failure at the transport rather than an answer from the server.
async fn connect_retrying(opts: ConnectOptions, what: &str) -> DatabaseConnection {
    let mut attempt = 1;
    loop {
        match Database::connect(opts.clone()).await {
            Ok(db) => return db,
            Err(e) if transient(&e) && attempt < LOCK_SESSION_ATTEMPTS => {
                attempt += 1;
                tokio::time::sleep(LOCK_SESSION_RETRY_DELAY).await;
            }
            Err(e) => panic!("Failed to connect to {what} in {attempt} attempts: {e}"),
        }
    }
}

/// Open the lock session, creating the database when it is not there yet.
///
/// The isolated-database recipe hands the suite an empty database and nothing more: the migrations
/// and `tests/fixtures/reference_rows.sql` are the harness's own work. So a name that does not
/// resolve is a database to make, not a run to fail; the alternative is every test in the binary
/// failing setup with `3D000` after the server was restarted under it.
async fn open_lock_session(url: &str) -> DatabaseConnection {
    let mut opts = ConnectOptions::new(lock_session_url(url));
    opts.max_connections(1)
        .min_connections(1)
        .sqlx_logging(false);
    let mut attempt = 1;
    loop {
        match Database::connect(opts.clone()).await {
            Ok(session) => return session,
            Err(e) if sqlstate(&e).as_deref() == Some(UNDEFINED_DATABASE) => {
                let name = crate::common::scratch::name_of(url);
                let server = crate::common::scratch::server(url).await;
                server
                    .execute_unprepared(&format!("CREATE DATABASE {name}"))
                    .await
                    .expect("create the test database");
                return Database::connect(opts).await.expect(
                    "Failed to open the harness lock session on the database just created",
                );
            }
            Err(e) if transient(&e) && attempt < LOCK_SESSION_ATTEMPTS => {
                attempt += 1;
                tokio::time::sleep(LOCK_SESSION_RETRY_DELAY).await;
            }
            Err(e) => {
                panic!("Failed to open the harness lock session in {attempt} attempts: {e}")
            }
        }
    }
}

/// Take the database for this process, waiting for whoever holds it.
///
/// `cleanup_test_db` truncates tables a concurrent suite is reading, so two runners on one database
/// produce failures that read like product defects. Waiting is the only honest answer: the second
/// runner is not wrong, it is early.
async fn hold_the_database(url: &str) {
    LOCK_SESSION
        .get_or_init(|| async {
            let session = open_lock_session(url).await;

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

/// Send the server's own `tracing` output to the test's stderr, once per binary.
///
/// A handler that fails logs what failed and answers `{"error":"Database error"}`; with no
/// subscriber installed the log goes nowhere, so a 500 in a test is a body with nothing behind
/// it. `RUST_LOG` selects what is shown and `--nocapture` is what lets it reach the terminal.
pub fn show_server_logs() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("river_db=error"));
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init();
    });
}

pub async fn setup_test_db() -> DatabaseConnection {
    dotenvy::dotenv().ok();
    show_server_logs();
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
    let db = connect_retrying(opts, "the test database").await;

    require_connection_headroom(&db, &url_for_message).await;

    migration::Migrator::up(&db, None)
        .await
        .expect("Failed to run migrations");

    stop_background_policies(&db).await;

    // The reference rows a deployment's operator authors for themselves (Q134: a database starts
    // blank). The suites that read them want them there, so the fixture installs them.
    exec_unprepared(&db, include_str!("../fixtures/reference_rows.sql")).await;

    // Every channel carries an instrument in the deployed system: registration mints one, and a
    // reading may not be stored without naming what measured it. A fixture stream built by raw SQL
    // has no source to mint from, so the suite gives it [`FIXTURE_SENSOR_ID`] as the column
    // default. A stream created through a route still sets the column itself, so the minting paths
    // are exercised, not defaulted around.
    db.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "ALTER TABLE data_streams ALTER COLUMN sensor_id SET DEFAULT '{FIXTURE_SENSOR_ID}'"
        ),
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
    "instrument_proposals",
    "pairing_plans",
    "data_streams",
    "meteoswiss_fetch_state",
    "meteoswiss_stations",
    "reprocessing_jobs",
    "schedules",
    "change_audit",
    "csv_import_staging",
    "csv_import_chunks",
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
    "calculation_formulas",
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

/// End work a finished test left running, so the TRUNCATE below cannot deadlock against it.
///
/// `stop_test_workers` covers the job workers this process owns. It does not cover a request
/// handler's own spawned work, and dropping a test's runtime aborts a task without rolling back
/// what its connection already sent, so a statement can still be in flight against `readings` or
/// `replicate_audit_holds` when the next test truncates. The TRUNCATE wants those relations in a
/// different order and the pair deadlocks.
///
/// Both `active` and `idle in transaction` qualify: a backend still executing its `UPDATE` reports
/// `active`, and that is the one that holds the row locks. Cleanup runs before a test has issued
/// anything of its own, so any such backend belongs to a test that has already finished. The lock
/// session is spared by name, because terminating it would release [`HARNESS_LOCK_KEY`].
///
/// Measured over a full `sync` run: this fires four times, every time on the detached
/// `UPDATE api_tokens SET last_used_at` the auth layer spawns per request. `api_tokens` is in the
/// TRUNCATE list, so that write is a deadlock partner, and no worker owns it.
async fn end_stranded_transactions(db: &DatabaseConnection) {
    exec(
        db,
        &format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
              WHERE datname = current_database() AND pid <> pg_backend_pid() \
                AND application_name IS DISTINCT FROM '{LOCK_SESSION_APPLICATION_NAME}' \
                AND state IN ('active', 'idle in transaction')"
        ),
    )
    .await;
}

pub async fn cleanup_test_db(db: &DatabaseConnection) {
    // The fixture's own writer goes first: a worker still claiming rows while this truncates is
    // the one thing in the harness that can write between the TRUNCATE and the seed.
    crate::common::stop_test_workers().await;
    end_stranded_transactions(db).await;
    let stmts = [
        "SELECT remove_continuous_aggregate_policy('readings_monthly', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_weekly', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_daily', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_twelve_hourly', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_six_hourly', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_hourly', if_not_exists => true)",
        // The rollups are materialized hypertables of their own, so truncating `readings` leaves
        // their buckets in place and the next test reading extents from a summary sees the
        // previous test's data.
        "TRUNCATE readings_hourly, readings_six_hourly, readings_twelve_hourly, readings_daily, \
                  readings_weekly, readings_monthly",
        // Tool scripts a test installed. The migration-seeded ones (`created_by = 'seed'`) are
        // reference data and stay; a fixture left enabled makes the next theme's visits recompute
        // against a calculation that theme never installed.
        "DELETE FROM tool_script_activations a USING tool_scripts s \
          WHERE a.tool_script_id = s.id AND s.created_by IS DISTINCT FROM 'seed'",
        "UPDATE tool_scripts SET active_version_id = NULL WHERE created_by IS DISTINCT FROM 'seed'",
        "DELETE FROM tool_script_versions v USING tool_scripts s \
          WHERE v.tool_script_id = s.id AND s.created_by IS DISTINCT FROM 'seed'",
        // A formula calculation owns `calculation_formulas` rows, which cascade with the
        // calculation; the sources hanging off them do not, so they go first or the delete below
        // is refused by their foreign key.
        "DELETE FROM derived_parameter_sources src \
           USING calculation_formulas d JOIN tool_scripts s ON s.id = d.tool_script_id \
           WHERE src.derived_definition_id = d.id AND s.created_by IS DISTINCT FROM 'seed'",
        "DELETE FROM tool_scripts WHERE created_by IS DISTINCT FROM 'seed'",
        "DELETE FROM parameter_group_members",
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
         CHECK ((sensor_id IS NOT NULL) OR (site_id IS NULL) \
                OR (measurement_type IS NOT DISTINCT FROM 'derived')) \
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

#[cfg(test)]
mod tests {
    use super::{DbErr, RuntimeErr, sqlstate, transient};

    fn connect_error(e: sea_orm::SqlxError) -> DbErr {
        DbErr::Conn(RuntimeErr::SqlxError(std::sync::Arc::new(e)))
    }

    #[test]
    fn test_transient_accepts_a_reset_at_the_socket() {
        let reset = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        assert!(transient(&connect_error(sea_orm::SqlxError::Io(reset))));
        assert!(transient(&connect_error(sea_orm::SqlxError::PoolTimedOut)));
    }

    #[test]
    fn test_transient_refuses_an_answer_from_the_server() {
        assert!(!transient(&connect_error(sea_orm::SqlxError::RowNotFound)));
        assert!(!transient(&DbErr::RecordNotFound("nothing".to_string())));
    }

    // The two classifications are read from the same error and must not overlap: a transport
    // failure carries no SQLSTATE, so it never takes the create-the-database branch.
    #[test]
    fn test_a_transient_error_carries_no_sqlstate() {
        let reset = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        assert_eq!(
            sqlstate(&connect_error(sea_orm::SqlxError::Io(reset))),
            None
        );
    }
}
