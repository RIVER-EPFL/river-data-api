//! A database of this test's own, built by the migrations.
//!
//! The shared harness database carries the reference rows every suite installs, and
//! `cleanup_test_db` removes the rollup refresh policies from it, so a suite about what the
//! migrations themselves build cannot read either from it. These build a database instead.

use sea_orm::{ConnectionTrait, Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;

/// `DATABASE_URL` with its database name replaced, so a scratch database is made on the same
/// server.
#[must_use]
pub fn url_for(base: &str, database: &str) -> String {
    let cut = base.rfind('/').expect("a database name in DATABASE_URL");
    let query = base[cut..]
        .find('?')
        .map(|q| &base[cut + q..])
        .unwrap_or("");
    format!("{}/{database}{query}", &base[..cut])
}

/// The database name in a connection URL.
#[must_use]
pub fn name_of(url: &str) -> &str {
    let cut = url.rfind('/').expect("a database name in DATABASE_URL") + 1;
    let tail = &url[cut..];
    tail.split('?').next().unwrap_or(tail)
}

/// A connection to the server's own `postgres` database, which is where a database is created and
/// dropped from.
pub async fn server(base: &str) -> DatabaseConnection {
    Database::connect(url_for(base, "postgres"))
        .await
        .expect("connect to the server")
}

/// A freshly created database with the migrations run on it. Any database of the same name is
/// discarded first, so a run that died before its teardown does not fail the next one.
pub async fn build(base: &str, server: &DatabaseConnection, name: &str) -> DatabaseConnection {
    discard(server, name).await;
    server
        .execute_unprepared(&format!("CREATE DATABASE {name}"))
        .await
        .expect("create the scratch database");
    let db = Database::connect(url_for(base, name))
        .await
        .expect("connect to the scratch database");
    migration::Migrator::up(&db, None)
        .await
        .expect("the migrations build a database of their own");
    db
}

/// Drop a scratch database, whoever is still connected to it.
pub async fn discard(server: &DatabaseConnection, name: &str) {
    server
        .execute_unprepared(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .await
        .expect("drop the scratch database");
}

#[cfg(test)]
mod tests {
    use super::{name_of, url_for};

    #[test]
    fn test_name_of_reads_the_database_from_the_url() {
        assert_eq!(
            name_of("postgresql://postgres:psql@localhost:5444/river_test"),
            "river_test"
        );
        assert_eq!(
            name_of("postgresql://postgres:psql@localhost:5444/river_test?sslmode=disable"),
            "river_test"
        );
    }

    #[test]
    fn test_url_for_keeps_the_query() {
        assert_eq!(
            url_for("postgresql://h:5444/river_test?sslmode=disable", "postgres"),
            "postgresql://h:5444/postgres?sslmode=disable"
        );
    }
}
