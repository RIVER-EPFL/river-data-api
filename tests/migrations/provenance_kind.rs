//! Scenario: a database carried forward holds readings from every write path and no origin column.
//!
//! Expected behaviour: the backfill stamps each row with the origin its own evidence proves, a row
//! whose stream is gone is stamped `migration` rather than guessed at, and the trigger keeps the
//! column total for a writer that names nothing.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use migration::m20260910_000006_provenance_kind::UP;

async fn restore_pre_migration_shape(db: &DatabaseConnection) {
    crate::common::exec_unprepared(
        db,
        "DROP TRIGGER IF EXISTS trg_readings_default_provenance_kind ON readings;
         ALTER TABLE readings DROP CONSTRAINT IF EXISTS readings_provenance_kind_check;
         ALTER TABLE readings DROP COLUMN IF EXISTS provenance_kind",
    )
    .await;
}

async fn seed_one_row_per_origin(db: &DatabaseConnection) {
    crate::common::seed_test_data(db).await;
    for (id, source, key) in [
        (
            "00000000-0000-4000-e900-000000000001",
            "grab_sample",
            "k-manual",
        ),
        ("00000000-0000-4000-e900-000000000002", "api", "k-batch"),
        ("00000000-0000-4000-e900-000000000003", "cnet", "k-sync"),
    ] {
        crate::common::seed_data_stream(db, id, source, key).await;
    }
    // The tool row: a stored blob is the record, and the blob says which path minted it.
    crate::common::exec(
        db,
        "INSERT INTO readings (stream_id, time, replicate_index, raw_value, measurement_type, provenance) \
         VALUES ('00000000-0000-4000-e900-000000000001', '2025-05-01T00:00:00Z', 0, 1.0, 'spot', \
                 '{\"source\": \"tool_run\", \"run_id\": \"x\"}'::jsonb)",
    )
    .await;
    crate::common::exec(
        db,
        "INSERT INTO readings (stream_id, time, replicate_index, raw_value, measurement_type) \
         VALUES ('00000000-0000-4000-e900-000000000001', '2025-05-01T01:00:00Z', 0, 2.0, 'spot')",
    )
    .await;
    crate::common::exec(
        db,
        "INSERT INTO readings (stream_id, time, replicate_index, raw_value) \
         VALUES ('00000000-0000-4000-e900-000000000002', '2025-05-01T02:00:00Z', 0, 3.0)",
    )
    .await;
    crate::common::exec(
        db,
        "INSERT INTO readings (stream_id, time, replicate_index, raw_value) \
         VALUES ('00000000-0000-4000-e900-000000000003', '2025-05-01T03:00:00Z', 0, 4.0)",
    )
    .await;
    crate::common::exec(
        db,
        "INSERT INTO readings (stream_id, time, replicate_index, raw_value, measurement_type) \
         VALUES ('00000000-0000-4000-e900-000000000003', '2025-05-01T04:00:00Z', 0, 5.0, 'derived')",
    )
    .await;
}

async fn kind_at(db: &DatabaseConnection, time: &str) -> String {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT provenance_kind AS v FROM readings WHERE time = '{time}'"),
    ))
    .await
    .expect("query")
    .expect("the reading")
    .try_get::<String>("", "v")
    .expect("provenance_kind")
}

#[tokio::test]
#[serial]
async fn the_backfill_stamps_each_row_with_what_it_can_prove() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    restore_pre_migration_shape(&db).await;
    seed_one_row_per_origin(&db).await;

    crate::common::exec_unprepared(&db, UP).await;

    assert_eq!(kind_at(&db, "2025-05-01T00:00:00Z").await, "tool_run");
    assert_eq!(kind_at(&db, "2025-05-01T01:00:00Z").await, "manual");
    assert_eq!(kind_at(&db, "2025-05-01T02:00:00Z").await, "batch");
    assert_eq!(kind_at(&db, "2025-05-01T03:00:00Z").await, "sync");
    assert_eq!(kind_at(&db, "2025-05-01T04:00:00Z").await, "derived");

    // A writer that names nothing after the migration is still stamped.
    crate::common::exec(
        &db,
        "INSERT INTO readings (stream_id, time, replicate_index, raw_value) \
         VALUES ('00000000-0000-4000-e900-000000000003', '2025-05-01T05:00:00Z', 0, 6.0)",
    )
    .await;
    assert_eq!(kind_at(&db, "2025-05-01T05:00:00Z").await, "sync");

    crate::common::cleanup_test_db(&db).await;
}
