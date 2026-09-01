//! Scenario: the `readings` compression helpers put chunks into the state the 30-day production
//! policy reaches, so tests can mutate a compressed chunk and assert on the decompression cap.

use chrono::{DateTime, Duration, Utc};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement, TransactionTrait};
use serial_test::serial;
use uuid::Uuid;

use crate::common::compression::{
    compress_readings_range, compressed_readings_chunk_count, connect_with_decompression_cap,
};
use crate::common::sensor_lifecycle::create_unpaired_stream;
use crate::common::{cleanup_test_db, exec, setup_test_db};

const ROWS: i64 = 50;

async fn seed_old_readings() -> (DatabaseConnection, Uuid, DateTime<Utc>) {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    let stream_id = create_unpaired_stream(&db, "compression_harness").await;
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
    (db, stream_id, base)
}

fn update_all(stream_id: Uuid) -> Statement {
    Statement::from_string(
        DatabaseBackend::Postgres,
        format!("UPDATE readings SET raw_value = raw_value + 1 WHERE stream_id = '{stream_id}'"),
    )
}

#[tokio::test]
#[serial]
async fn test_compress_readings_range_compresses_matching_chunks() {
    let (db, _stream_id, base) = seed_old_readings().await;
    assert_eq!(compressed_readings_chunk_count(&db).await, 0);

    let window = (base - Duration::days(1), base + Duration::days(1));
    assert!(compress_readings_range(&db, window.0, window.1).await >= 1);
    assert!(compressed_readings_chunk_count(&db).await >= 1);

    // A second pass re-covers the same chunks, recompressing any rows staged since; an empty
    // window is a no-op.
    assert!(compress_readings_range(&db, window.0, window.1).await >= 1);
    assert_eq!(
        compress_readings_range(&db, Utc::now() + Duration::days(1), Utc::now() + Duration::days(2)).await,
        0
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn test_decompression_cap_rejects_unguarded_update_on_compressed_chunk() {
    let (db, stream_id, base) = seed_old_readings().await;
    compress_readings_range(&db, base - Duration::days(1), base + Duration::days(1)).await;

    let capped = connect_with_decompression_cap(20).await;
    assert!(
        capped.execute(update_all(stream_id)).await.is_err(),
        "an unguarded UPDATE of {ROWS} rows should exceed a 20-tuple decompression cap"
    );

    let txn = capped.begin().await.expect("begin");
    txn.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0",
    ))
    .await
    .expect("cap lift");
    txn.execute(update_all(stream_id))
        .await
        .expect("guarded UPDATE over a compressed chunk");
    txn.commit().await.expect("commit");

    cleanup_test_db(&db).await;
}
