//! The request pool's statement timeout is a property of every backend it opens, not of whichever
//! connection happened to be configured once at startup.

use std::collections::HashSet;

use river_db::common::db_pool::{connect_background_pool, connect_request_pool};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use crate::common::test_config;

/// `(statement_timeout, backend pid)` as the backend serving this query reports them.
async fn backend_settings(db: &DatabaseConnection) -> (String, i32) {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            // The sleep holds the backend so the concurrent callers cannot be served one after
            // another on a single connection.
            "SELECT current_setting('statement_timeout') AS timeout, \
                    pg_backend_pid() AS pid, pg_sleep(0.2)"
                .to_string(),
        ))
        .await
        .expect("settings query")
        .expect("one row");
    (
        row.try_get("", "timeout").expect("timeout"),
        row.try_get("", "pid").expect("pid"),
    )
}

#[tokio::test]
#[serial]
async fn every_request_pool_backend_carries_the_timeout() {
    let config = test_config();
    let db = connect_request_pool(&config)
        .await
        .expect("request pool connects");

    let results = futures::future::join_all((0..8).map(|_| {
        let db = db.clone();
        async move { backend_settings(&db).await }
    }))
    .await;

    let pids: HashSet<i32> = results.iter().map(|(_, pid)| *pid).collect();
    assert_eq!(pids.len(), 8, "the eight callers shared backends: {pids:?}");
    for (timeout, pid) in &results {
        assert_eq!(
            timeout, "1min",
            "backend {pid} runs unbounded; the timeout reached only some connections"
        );
    }
}

#[tokio::test]
#[serial]
async fn the_background_pool_has_no_ceiling() {
    let config = test_config();
    let db = connect_background_pool(&config)
        .await
        .expect("background pool connects");

    let (timeout, _) = backend_settings(&db).await;
    assert_eq!(
        timeout, "0",
        "migrations and jobs legitimately run for minutes"
    );
}
