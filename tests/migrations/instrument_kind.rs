//! Scenario: a database carried forward holds instruments minted by all four paths and no `kind`
//! column to tell them apart.
//!
//! Expected behaviour: each row is classified by the evidence its own minting left, the four
//! classes are exactly the ones the constraint admits, and a row matching no rule is a device
//! rather than whatever the column happens to default to.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use migration::m20260907_000005_instrument_kind::UP;

async fn restore_pre_migration_shape(db: &DatabaseConnection) {
    crate::common::exec_unprepared(
        db,
        "ALTER TABLE sensors DROP CONSTRAINT IF EXISTS sensors_kind_check;
         ALTER TABLE sensors DROP COLUMN IF EXISTS kind",
    )
    .await;
}

/// One row per minting path, as each path leaves it: the two internal entry channels by their
/// source system, the source's per-parameter instrument by the note the minting writes, a lab
/// device by its flag alone, and a field probe with none of those marks.
async fn seed_one_row_per_minting_path(db: &DatabaseConnection) {
    crate::common::exec(
        db,
        "INSERT INTO sensors (id, name, source_system, source_key, is_lab_instrument, metadata) VALUES \
         ('00000000-0000-4000-ea00-000000000001', 'Grab entry', 'grab_sample', 'grab_sample:s:p', true, NULL), \
         ('00000000-0000-4000-ea00-000000000002', 'Batch entry', 'api', 'api:s:p', true, NULL), \
         ('00000000-0000-4000-ea00-000000000003', 'DOC (cnet)', 'cnet', 'cnet:DOC', true, \
          '{\"minted_from_stream\": \"cnet:FP1:DOC\"}'::jsonb), \
         ('00000000-0000-4000-ea00-000000000004', 'Bench analyser', 'manual', 'manual:analyser', true, NULL), \
         ('00000000-0000-4000-ea00-000000000005', 'Martigny Depth', 'vaisala', 'vaisala:1270', false, NULL)",
    )
    .await;
}

async fn kind_of(db: &DatabaseConnection, id: &str) -> String {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT kind FROM sensors WHERE id = '{id}'"),
    ))
    .await
    .expect("query")
    .expect("the instrument")
    .try_get::<String>("", "kind")
    .expect("kind")
}

#[tokio::test]
#[serial]
async fn each_minting_path_is_classified_by_its_own_evidence() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    restore_pre_migration_shape(&db).await;
    seed_one_row_per_minting_path(&db).await;

    crate::common::exec_unprepared(&db, UP).await;

    assert_eq!(
        kind_of(&db, "00000000-0000-4000-ea00-000000000001").await,
        "entry_channel",
        "a hand-typed value is entered through a channel, not measured by the deployed probe"
    );
    assert_eq!(
        kind_of(&db, "00000000-0000-4000-ea00-000000000002").await,
        "entry_channel"
    );
    assert_eq!(
        kind_of(&db, "00000000-0000-4000-ea00-000000000003").await,
        "source_parameter",
        "the source's instrument for one parameter across every station"
    );
    assert_eq!(
        kind_of(&db, "00000000-0000-4000-ea00-000000000004").await,
        "lab",
        "a lab device carries no minting note, so the flag is what is left to read"
    );
    assert_eq!(
        kind_of(&db, "00000000-0000-4000-ea00-000000000005").await,
        "device",
        "a probe matches no rule and stays a device"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// The classification is ordered, and the order is what keeps an entry channel from being read as
/// a lab instrument: three of the four paths set `is_lab_instrument`, so that flag alone cannot
/// separate them.
#[tokio::test]
#[serial]
async fn a_lab_flagged_entry_channel_is_still_an_entry_channel() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    restore_pre_migration_shape(&db).await;
    seed_one_row_per_minting_path(&db).await;

    crate::common::exec_unprepared(&db, UP).await;

    let lab_flagged: i64 = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*) AS n FROM sensors WHERE is_lab_instrument IS TRUE AND kind = 'lab'"
                .to_string(),
        ))
        .await
        .expect("query")
        .expect("a row")
        .try_get("", "n")
        .expect("n");
    assert_eq!(
        lab_flagged, 1,
        "four rows carry the flag and exactly one of them is a lab instrument"
    );

    // The constraint the migration leaves admits the four classes and nothing else.
    let refused = db
        .execute_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE sensors SET kind = 'probe' \
             WHERE id = '00000000-0000-4000-ea00-000000000005'"
                .to_string(),
        ))
        .await;
    assert!(
        refused.is_err(),
        "a fifth class is refused by sensors_kind_check"
    );

    crate::common::cleanup_test_db(&db).await;
}
