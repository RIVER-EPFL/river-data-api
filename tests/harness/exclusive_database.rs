//! Two suites sharing one database is the harness failing, not the code. `cleanup_test_db`
//! truncates tables the other run is querying, so its readings vanish mid-test and its queries
//! queue behind an ACCESS EXCLUSIVE lock until the pool has no connection left to hand out. The
//! failures that produces (a 404 for a site that existed, a pool acquire timing out) read exactly
//! like product defects. The harness takes the database, so a second runner waits instead.

use sea_orm::{ConnectionTrait, Database, Statement};
use serial_test::serial;

use crate::common::db::HARNESS_LOCK_KEY;
use crate::common::{setup_test_db, test_config};

#[tokio::test]
#[serial]
async fn a_second_runner_cannot_take_the_database_this_one_holds() {
    let _db = setup_test_db().await;

    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    let other = Database::connect(&url)
        .await
        .expect("second session connects");
    let row = other
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT pg_try_advisory_lock({HARNESS_LOCK_KEY}) AS got"),
        ))
        .await
        .expect("try lock")
        .expect("one row");
    let got: bool = row.try_get("", "got").expect("got");

    assert!(
        !got,
        "a concurrent suite took the same database; its truncations would be read as defects"
    );
}

#[tokio::test]
#[serial]
async fn the_harness_pool_has_the_ceiling_the_deployment_runs_on() {
    let db = setup_test_db().await;
    let ceiling = db
        .get_postgres_connection_pool()
        .options()
        .get_max_connections();

    assert_eq!(
        ceiling,
        test_config().db_max_connections,
        "the harness runs handlers, the job worker and the test's own queries on one pool; \
         a ceiling below the deployment's turns a burst into an acquire timeout"
    );
}
