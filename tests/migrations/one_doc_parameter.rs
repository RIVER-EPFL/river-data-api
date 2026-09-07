//! Scenario: a database carried forward holds the baseline's `DOC` catalog row, and the portal's
//! own code for the same analyte is `DOC_ppb`.
//!
//! Expected behaviour: the row takes the portal's code, and a database that already holds both
//! keeps both, because merging them moves readings and is an operator action.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use migration::m20260908_000006_one_doc_parameter::UP;


async fn codes(db: &DatabaseConnection) -> String {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT COALESCE(string_agg(code, ',' ORDER BY code), '') AS codes FROM parameters \
          WHERE code IN ('DOC', 'DOC_ppb')"
            .to_string(),
    ))
    .await
    .expect("query")
    .expect("a row")
    .try_get("", "codes")
    .expect("codes")
}

async fn seed_parameter(db: &DatabaseConnection, code: &str) {
    crate::common::exec_unprepared(
        db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
             VALUES (gen_random_uuid(), '{code}', '{code}', 'ppb', 'measurement')"
        ),
    )
    .await;
}

#[tokio::test]
#[serial]
async fn the_seeded_row_takes_the_portal_s_code() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    seed_parameter(&db, "DOC").await;

    crate::common::exec_unprepared(&db, UP).await;

    assert_eq!(codes(&db).await, "DOC_ppb");

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn a_database_holding_both_keeps_both() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    seed_parameter(&db, "DOC").await;
    seed_parameter(&db, "DOC_ppb").await;

    crate::common::exec_unprepared(&db, UP).await;

    // Renaming would collide, and deleting would take whatever readings the row carries with it.
    assert_eq!(codes(&db).await, "DOC,DOC_ppb");

    crate::common::cleanup_test_db(&db).await;
}
