//! Scenario: a test's job worker is still inside a transaction when the test ends.
//!
//! Expected behaviour: the next test's cleanup ends it before truncating. A worker's shutdown
//! future is `future::pending`, so it stops only when the runtime drops, and a runtime drop does
//! not roll back what the connection already sent. The stranded backend holds locks on the same
//! relations the TRUNCATE wants, in the other order, and the pair deadlocks.

use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use serial_test::serial;

use crate::common::{cleanup_test_db, setup_test_db};

async fn open_transactions(db: &DatabaseConnection) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT count(*) AS n FROM pg_stat_activity \
          WHERE datname = current_database() AND pid <> pg_backend_pid() \
            AND state = 'idle in transaction'"
            .to_string(),
    ))
    .await
    .expect("query")
    .expect("row")
    .try_get::<i64>("", "n")
    .expect("count")
}

#[tokio::test]
#[serial]
async fn cleanup_ends_a_transaction_the_previous_test_left_open() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;

    // A second connection standing in for the worker a dropped runtime left mid-job: it takes the
    // lock the TRUNCATE needs first and then stops, which is the shape that deadlocks.
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let stranded = Database::connect(&url).await.expect("second connection");
    stranded
        .execute_unprepared("BEGIN; LOCK TABLE readings IN ROW EXCLUSIVE MODE;")
        .await
        .expect("open a transaction holding readings");
    assert_eq!(
        open_transactions(&db).await,
        1,
        "the stranded transaction is open before cleanup"
    );

    // Bounded: without the sweep the TRUNCATE waits on that lock instead of failing, so an
    // unbounded call would hang the suite rather than report the guard is gone.
    tokio::time::timeout(std::time::Duration::from_secs(30), cleanup_test_db(&db))
        .await
        .expect("cleanup ends the stranded transaction instead of queueing behind its lock");

    assert_eq!(
        open_transactions(&db).await,
        0,
        "cleanup ends transactions left open by a finished test"
    );
}
