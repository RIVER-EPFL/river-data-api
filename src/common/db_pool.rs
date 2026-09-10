//! The two connection pools this process runs on, and the one place their difference is stated.
//!
//! A request handler's query is bounded by `statement_timeout`, set equal to the HTTP request
//! timeout because the two are one contract: a query still running when the response has already
//! timed out is a runaway holding a backend nobody is waiting for. Migrations and background jobs
//! legitimately run for minutes, so they draw from a second, small pool with no ceiling.
//!
//! The ceiling travels in the connection options rather than a `SET` on an established connection.
//! `SET` without `LOCAL` is session-scoped, so it reaches one backend and none of the ones the pool
//! opens later.

use std::time::Duration;

use sea_orm::{ConnectOptions, Database, DatabaseConnection, DbErr};

use crate::config::Config;

/// Connections the background pool holds. Two callers share it, the migrator at boot and the job
/// worker loop, and the worker runs one job at a time.
const BACKGROUND_CONNECTIONS: u32 = 3;

/// The connection URL with a `statement_timeout` every backend opened from it inherits.
fn with_statement_timeout(database_url: &str, seconds: u64) -> String {
    let separator = if database_url.contains('?') { '&' } else { '?' };
    format!("{database_url}{separator}options=-c%20statement_timeout%3D{seconds}s")
}

/// The pool every request handler draws from.
pub async fn connect_request_pool(config: &Config) -> Result<DatabaseConnection, DbErr> {
    let mut opts = ConnectOptions::new(with_statement_timeout(
        &config.database_url,
        config.request_timeout_seconds,
    ));
    opts.max_connections(config.db_max_connections)
        .min_connections(config.db_min_connections)
        .connect_timeout(Duration::from_secs(5))
        // Without an explicit acquire timeout SeaORM reuses connect_timeout, so a saturated pool
        // fails callers after 5s instead of queueing them behind a burst. 30s means a burst
        // degrades to latency, not to 500s.
        .acquire_timeout(Duration::from_secs(30))
        // A backend's first query against the readings hypertable plans in ~240ms while it loads
        // TimescaleDB's chunk metadata, and ~5ms after. Long enough that the warm set survives a
        // quiet period rather than being recycled into cold connections.
        .idle_timeout(Duration::from_secs(1800))
        .sqlx_logging(false)
        .set_schema_search_path("public");
    Database::connect(opts).await
}

/// The pool the migrator and the job worker draw from, unbounded because their work is measured in
/// minutes. A migration that needs a narrower ceiling still sets its own `SET LOCAL`.
pub async fn connect_background_pool(config: &Config) -> Result<DatabaseConnection, DbErr> {
    let mut opts = ConnectOptions::new(&config.database_url);
    opts.max_connections(BACKGROUND_CONNECTIONS)
        .min_connections(1)
        .connect_timeout(Duration::from_secs(5))
        .acquire_timeout(Duration::from_secs(30))
        .idle_timeout(Duration::from_secs(1800))
        .sqlx_logging(false)
        .set_schema_search_path("public");
    Database::connect(opts).await
}

#[cfg(test)]
#[path = "tests/db_pool.rs"]
mod tests;
