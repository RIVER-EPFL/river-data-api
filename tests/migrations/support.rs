//! Harness for running migrations against a database that has rows in it.

use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;

/// A database of this test's own, created fresh from the server `DATABASE_URL` points at.
///
/// A partially migrated database is unusable by anything else, so it cannot be shared: the name is
/// the caller's and the database is rebuilt at the start of every run rather than cleaned at the
/// end, which also survives a run that panicked halfway.
pub async fn fresh_database(name: &str) -> DatabaseConnection {
    dotenvy::dotenv().ok();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    let (server, _existing) = url
        .rsplit_once('/')
        .unwrap_or_else(|| panic!("DATABASE_URL names no database: {url}"));

    let admin = Database::connect(format!("{server}/postgres"))
        .await
        .expect("connect to the postgres database to build a throwaway one");
    exec(
        &admin,
        &format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
    )
    .await;
    exec(&admin, &format!("CREATE DATABASE {name}")).await;
    admin.close().await.ok();

    Database::connect(format!("{server}/{name}"))
        .await
        .expect("connect to the throwaway database")
}

/// Apply migrations up to and including `name`, leaving everything after it pending.
///
/// The stop point is a migration name rather than a count, so inserting a migration anywhere in the
/// list moves it rather than silently changing which schema the test populates.
pub async fn migrate_through(db: &DatabaseConnection, name: &str) {
    let steps = u32::try_from(position_of(name) + 1).expect("migration count fits a u32");
    migration::Migrator::up(db, Some(steps))
        .await
        .unwrap_or_else(|e| panic!("migrating through {name} failed: {e}"));

    assert!(
        applied(db, name).await,
        "{name} should be applied after migrating through it"
    );
    if let Some(next) = migration::Migrator::migrations()
        .get(position_of(name) + 1)
        .map(|m| m.name().to_string())
    {
        assert!(
            !applied(db, &next).await,
            "{next} should still be pending: the fixture is written against the schema {name} leaves"
        );
    }
}

/// How many migrations `name` is followed by, itself included: the step count that rolls it back.
#[must_use]
pub fn steps_back_through(name: &str) -> u32 {
    let total = migration::Migrator::migrations().len();
    u32::try_from(total - position_of(name)).expect("migration count fits a u32")
}

fn position_of(name: &str) -> usize {
    migration::Migrator::migrations()
        .iter()
        .position(|m| m.name() == name)
        .unwrap_or_else(|| panic!("no migration named {name}"))
}

async fn applied(db: &DatabaseConnection, name: &str) -> bool {
    db.query_one(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT 1 AS one FROM seaql_migrations WHERE version = $1",
        [name.into()],
    ))
    .await
    .expect("read the applied-migration log")
    .is_some()
}

pub async fn exec(db: &DatabaseConnection, sql: &str) {
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("SQL failed: {e}\nQuery: {sql}"));
}

/// One value from a query that returns a single row and a single column named `v`.
pub async fn scalar<T>(db: &DatabaseConnection, sql: &str) -> Option<T>
where
    T: sea_orm::TryGetable,
{
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("SQL failed: {e}\nQuery: {sql}"))
    .map(|row| row.try_get::<Option<T>>("", "v").expect("column v"))
    .unwrap_or(None)
}

pub async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    scalar::<i64>(db, sql).await.unwrap_or(0)
}

/// Whether a column is still on a table, for asserting that a migration dropped one.
pub async fn column_exists(db: &DatabaseConnection, table: &str, column: &str) -> bool {
    count(
        db,
        &format!(
            "SELECT count(*) AS v FROM information_schema.columns \
             WHERE table_name = '{table}' AND column_name = '{column}'"
        ),
    )
    .await
        > 0
}
