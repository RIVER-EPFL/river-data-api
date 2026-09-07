//! Scenario: a database carried forward holds portal instruments named after whichever station
//! registered them first.
//!
//! Expected behaviour: a source-parameter or lab row is renamed for its parameter and its source,
//! a device row keeps the slot it is stationed at, and no identity moves.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use migration::m20260910_000005_source_parameter_instrument_names::UP;

async fn seed_station_named_instruments(db: &DatabaseConnection) {
    crate::common::exec_unprepared(
        db,
        "INSERT INTO sensors (id, name, source_system, source_key, kind, is_lab_instrument) VALUES \
         ('11111111-1111-1111-1111-111111111111', 'FP1 DOC_avg_ppb', 'cnet', \
          'cnet:DOC_avg_ppb', 'source_parameter', true), \
         ('22222222-2222-2222-2222-222222222222', 'FP1 DOC', 'cnet', 'cnet:DOC', 'lab', true), \
         ('33333333-3333-3333-3333-333333333333', 'Martigny Depth', 'vaisala', \
          'vaisala:1270', 'device', false)",
    )
    .await;
}

async fn name_of(db: &DatabaseConnection, id: &str) -> String {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT name, source_key FROM sensors WHERE id = '{id}'"),
    ))
    .await
    .expect("query")
    .expect("the instrument")
    .try_get::<String>("", "name")
    .expect("name")
}

#[tokio::test]
#[serial]
async fn a_portal_instrument_loses_its_station_prefix() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    seed_station_named_instruments(&db).await;

    crate::common::exec_unprepared(&db, UP).await;

    assert_eq!(
        name_of(&db, "11111111-1111-1111-1111-111111111111").await,
        "DOC_avg_ppb (cnet)"
    );
    assert_eq!(
        name_of(&db, "22222222-2222-2222-2222-222222222222").await,
        "DOC (cnet)"
    );
    assert_eq!(
        name_of(&db, "33333333-3333-3333-3333-333333333333").await,
        "Martigny Depth",
        "a probe is stationed at its site and keeps the slot name"
    );

    let keys_intact: i64 = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*) AS n FROM sensors WHERE source_key IN \
             ('cnet:DOC_avg_ppb', 'cnet:DOC', 'vaisala:1270')"
                .to_string(),
        ))
        .await
        .expect("query")
        .expect("a row")
        .try_get("", "n")
        .expect("n");
    assert_eq!(keys_intact, 3, "identities are untouched");

    crate::common::cleanup_test_db(&db).await;
}
