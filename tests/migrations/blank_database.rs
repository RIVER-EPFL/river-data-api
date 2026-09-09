//! Scenario: an operator installs river-data and opens it before validating anything.
//!
//! Expected behaviour: the database the migrations build is blank. Every parameter, group,
//! constant, tool script and instrument arrives through a plan or a form somebody at this lab
//! agreed to (Q134), so a migration that seeds one puts rows nobody reviewed in front of them
//! with nothing on the page saying where they came from.
//!
//! Run: cargo test --test migrations blank_database -- --test-threads=1

use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
use serial_test::serial;

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

/// The URL with its database name replaced, so the scratch database is made on the same server.
fn url_for(base: &str, database: &str) -> String {
    let cut = base.rfind('/').expect("a database name in DATABASE_URL");
    let query = base[cut..].find('?').map(|q| &base[cut + q..]).unwrap_or("");
    format!("{}/{database}{query}", &base[..cut])
}

#[tokio::test]
#[serial]
async fn a_migrated_database_holds_no_rows_nobody_asked_for() {
    dotenvy::dotenv().ok();
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    // Its own database: the shared one carries the reference rows the suites install, which is
    // exactly the state this test exists to distinguish from what the migrations build.
    let scratch = format!("river_blank_{}", std::process::id());

    let admin = Database::connect(url_for(&base, "postgres"))
        .await
        .expect("connect to the server");
    let drop = format!("DROP DATABASE IF EXISTS {scratch} WITH (FORCE)");
    admin.execute_unprepared(&drop).await.expect("drop");
    admin
        .execute_unprepared(&format!("CREATE DATABASE {scratch}"))
        .await
        .expect("create the scratch database");

    let db = Database::connect(url_for(&base, &scratch))
        .await
        .expect("connect to the scratch database");
    migration::Migrator::up(&db, None)
        .await
        .expect("the migrations build a database of their own");

    let mut filled = Vec::new();
    for table in MUST_BE_EMPTY {
        let n = count(&db, table).await;
        if n > 0 {
            filled.push(format!("{table}: {n}"));
        }
    }
    let filled_report = filled.join(", ");

    db.close().await.expect("close the scratch connection");
    admin.execute_unprepared(&drop).await.expect("drop");

    assert!(
        filled.is_empty(),
        "a migration seeded rows an operator never validated: {filled_report}"
    );
}
