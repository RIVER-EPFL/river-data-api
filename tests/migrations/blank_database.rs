//! Scenario: an operator installs river-data and opens it before validating anything.
//!
//! Expected behaviour: the database the migrations build is blank. Every parameter, group,
//! constant, tool script and instrument arrives through a plan or a form somebody at this lab
//! agreed to (Q134), so a migration that seeds one puts rows nobody reviewed in front of them
//! with nothing on the page saying where they came from.
//!
//! Run: cargo test --test migrations blank_database -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use crate::common::scratch;

/// The tables an operator fills, in the order a reader would ask about them.
const MUST_BE_EMPTY: &[&str] = &[
    "parameters",
    "parameter_groups",
    "parameter_group_members",
    "constants",
    "tool_scripts",
    "tool_script_versions",
    "sensors",
    "projects",
    "sites",
];

async fn count(db: &DatabaseConnection, table: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT count(*) AS n FROM public.{table}"),
    ))
    .await
    .expect("count")
    .expect("a row")
    .try_get::<i64>("", "n")
    .expect("n")
}

#[tokio::test]
#[serial]
async fn a_migrated_database_holds_no_rows_nobody_asked_for() {
    dotenvy::dotenv().ok();
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    // Its own database: the shared one carries the reference rows the suites install, which is
    // exactly the state this test exists to distinguish from what the migrations build.
    let name = format!("river_blank_{}", std::process::id());

    let server = scratch::server(&base).await;
    let db = scratch::build(&base, &server, &name).await;

    let mut filled = Vec::new();
    for table in MUST_BE_EMPTY {
        let n = count(&db, table).await;
        if n > 0 {
            filled.push(format!("{table}: {n}"));
        }
    }
    let filled_report = filled.join(", ");

    db.close().await.expect("close the scratch connection");
    scratch::discard(&server, &name).await;

    assert!(
        filled.is_empty(),
        "a migration seeded rows an operator never validated: {filled_report}"
    );
}
