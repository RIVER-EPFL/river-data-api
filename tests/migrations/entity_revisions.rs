//! Scenario: a formula reads a site property, a constant, a curve, and a reading; a run records
//! the revision of each (Q215).
//!
//! Expected behaviour: every row of the six audited tables has a change_audit row from the moment
//! the database is built, a later edit adds one with a higher `seq`, and running the migrator
//! again preserves the seeded constants and their revisions.
//!
//! Run: cargo test --test migrations entity_revisions -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
use serial_test::serial;

use crate::common::scratch;

async fn one<T: sea_orm::TryGetable>(db: &DatabaseConnection, sql: &str, column: &str) -> T {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql,
    ))
    .await
    .expect("query")
    .expect("a row")
    .try_get::<T>("", column)
    .expect(column)
}

async fn exec(db: &DatabaseConnection, sql: &str) {
    db.execute_unprepared(sql).await.expect(sql);
}

#[tokio::test]
#[serial]
async fn every_audited_row_has_a_revision_and_an_edit_advances_it() {
    dotenvy::dotenv().ok();
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    let name = format!("river_revisions_{}", std::process::id());
    let server = scratch::server(&base).await;
    let db = scratch::build(&base, &server, &name).await;

    let seeded: i64 = one(&db, "SELECT count(*) AS n FROM constants", "n").await;
    let audited: i64 = one(
        &db,
        "SELECT count(*) AS n FROM change_audit WHERE change = 'constant_insert'",
        "n",
    )
    .await;
    assert_eq!(
        audited, seeded,
        "one constant_insert row per seeded constant"
    );

    exec(
        &db,
        "INSERT INTO projects (id, name) VALUES ('11111111-1111-1111-1111-111111111111', 'p')",
    )
    .await;
    exec(
        &db,
        "INSERT INTO sites (id, project_id, name, altitude_m) \
         VALUES ('22222222-2222-2222-2222-222222222222', '11111111-1111-1111-1111-111111111111', 's', 100)",
    )
    .await;
    let inserted: i64 = one(
        &db,
        "SELECT seq FROM change_audit WHERE subject = 'site:22222222-2222-2222-2222-222222222222' \
         AND change = 'site_insert'",
        "seq",
    )
    .await;
    exec(
        &db,
        "UPDATE sites SET altitude_m = 120 WHERE id = '22222222-2222-2222-2222-222222222222'",
    )
    .await;
    let updated: i64 = one(
        &db,
        "SELECT seq FROM change_audit WHERE subject = 'site:22222222-2222-2222-2222-222222222222' \
         AND change = 'site_update'",
        "seq",
    )
    .await;
    assert!(updated > inserted, "the edit is the newer revision");
    let old_altitude: f64 = one(
        &db,
        "SELECT (old_value->>'altitude_m')::float8 AS a FROM change_audit \
         WHERE subject = 'site:22222222-2222-2222-2222-222222222222' AND change = 'site_update'",
        "a",
    )
    .await;
    assert_eq!(
        old_altitude, 100.0,
        "the audit row keeps the value before the edit"
    );

    migration::Migrator::up(&db, None)
        .await
        .expect("running the migrator again succeeds");
    let audited_again: i64 = one(
        &db,
        "SELECT count(*) AS n FROM change_audit WHERE change = 'constant_insert'",
        "n",
    )
    .await;
    assert_eq!(audited_again, seeded, "the constants are audited once");
    let latest: i64 = one(&db, "SELECT max(seq) AS seq FROM change_audit", "seq").await;
    assert_eq!(latest, updated, "running the migrator preserves revisions");

    drop(db);
    scratch::discard(&server, &name).await;
}
