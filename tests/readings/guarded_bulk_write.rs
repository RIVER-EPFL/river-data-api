//! Scenario: bulk DML against `readings` reaches chunks the compression policy has already
//! compressed, on a server carrying a decompression cap.
//!
//! Expected behaviour: `common::bulk_write` lifts the cap and owns the transaction, so a statement
//! routed through it completes where the bare statement fails, reports the rows and time span it
//! touched, and leaves nothing behind when the work fails.

use chrono::{DateTime, Duration, Utc};
use river_db::common::bulk_write;
use river_db::error::AppError;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use crate::common::compression::{
    compress_readings_range, compressed_readings_chunk_count, connect_with_decompression_cap,
};
use crate::common::sensor_lifecycle::create_unpaired_stream;
use crate::common::{cleanup_test_db, exec, setup_test_db};

const ROWS: i64 = 40;

/// Compressed historical readings plus a connection whose sessions cap decompression at one tuple.
async fn seed_compressed(
    key: &str,
) -> (DatabaseConnection, DatabaseConnection, Uuid, DateTime<Utc>) {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    let stream_id = create_unpaired_stream(&db, key).await;
    let base = Utc::now() - Duration::days(400);
    for i in 0..ROWS {
        exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, time, raw_value, replicate_index) \
                 VALUES ('{stream_id}', '{}', {i}.0, 0)",
                (base + Duration::minutes(i)).to_rfc3339()
            ),
        )
        .await;
    }
    let compressed = compress_readings_range(&db, base, base + Duration::minutes(ROWS)).await;
    assert!(compressed > 0, "the seeded range should hold a chunk");
    assert!(compressed_readings_chunk_count(&db).await > 0);

    let capped = connect_with_decompression_cap(1).await;
    (db, capped, stream_id, base)
}

fn bump_all(stream_id: Uuid) -> Statement {
    Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "UPDATE readings SET raw_value = raw_value + 1 WHERE stream_id = $1",
        [stream_id.into()],
    )
}

async fn sum_of(db: &DatabaseConnection, stream_id: Uuid) -> f64 {
    let row = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT COALESCE(SUM(raw_value), 0)::float8 AS total FROM readings WHERE stream_id = $1",
            [stream_id.into()],
        ))
        .await
        .expect("sum query failed")
        .expect("sum returned no row");
    row.try_get("", "total").expect("total column")
}

#[tokio::test]
#[serial]
async fn guarded_mutation_completes_where_the_bare_statement_hits_the_decompression_cap() {
    let (db, capped, stream_id, base) = seed_compressed("guarded_bulk_write_cap").await;

    let bare = capped.execute(bump_all(stream_id)).await;
    assert!(
        bare.is_err(),
        "an uncapped bare UPDATE would prove nothing; the cap must bite first"
    );

    let touched = bulk_write::guarded_mutation(&capped, bump_all(stream_id))
        .await
        .expect("the guarded write lifts the cap");

    assert_eq!(touched.rows, u64::try_from(ROWS).unwrap());
    let (min_time, max_time) = touched.span().expect("a non-empty write reports its span");
    assert_eq!(min_time.timestamp(), base.timestamp());
    assert_eq!(
        max_time.timestamp(),
        (base + Duration::minutes(ROWS - 1)).timestamp()
    );
    // 0..39 summed, then one added to each row.
    assert!((sum_of(&db, stream_id).await - (780.0 + ROWS as f64)).abs() < 1e-9);

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn a_failed_guarded_write_leaves_nothing_committed() {
    let (db, capped, stream_id, _) = seed_compressed("guarded_bulk_write_rollback").await;
    let before = sum_of(&db, stream_id).await;

    let outcome: Result<(), AppError> = bulk_write::guarded(&capped, async |txn| {
        bulk_write::mutation(txn, bump_all(stream_id)).await?;
        Err(AppError::Internal("second statement failed".to_string()))
    })
    .await;

    assert!(outcome.is_err());
    assert!(
        (sum_of(&db, stream_id).await - before).abs() < 1e-9,
        "the first statement must roll back with the failed one"
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn a_mutation_matching_nothing_reports_an_empty_range() {
    let (db, capped, _, _) = seed_compressed("guarded_bulk_write_empty").await;

    let touched = bulk_write::guarded_mutation(&capped, bump_all(Uuid::new_v4()))
        .await
        .expect("a statement matching nothing is not an error");

    assert_eq!(touched.rows, 0);
    assert!(touched.is_empty());
    assert!(touched.span().is_none());

    cleanup_test_db(&db).await;
}
