//! Helpers for putting `readings` chunks into the compressed state that the production compression
//! policy (30 days, segmented by `stream_id`) reaches only after a chunk has aged out.

use chrono::{DateTime, Utc};
use sea_orm::{
    ConnectionTrait, Database, DatabaseBackend, DatabaseConnection, QueryResult, Statement,
};

/// Compress the `readings` chunks overlapping `[start, end]`, returning how many chunks now hold
/// the range compressed (0 when the range holds no chunks). A chunk that is already compressed is
/// passed to `compress_chunk` again: rows inserted after a chunk was compressed sit in its
/// uncompressed staging area until recompression, and with wide chunk intervals two test dates
/// routinely share one chunk.
pub async fn compress_readings_range(
    db: &DatabaseConnection,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> usize {
    let chunks = db
        .query_all(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT format('%I.%I', chunk_schema, chunk_name) AS chunk \
             FROM timescaledb_information.chunks \
             WHERE hypertable_name = 'readings' \
               AND range_end > $1 AND range_start <= $2 \
             ORDER BY range_start",
            [start.into(), end.into()],
        ))
        .await
        .expect("readings chunk lookup failed");
    compress_each(db, chunks).await
}

/// Compress every uncompressed `readings` chunk whose data is entirely older than `interval`
/// (a Postgres interval literal such as `"30 days"`), returning how many were compressed.
pub async fn compress_readings_older_than(db: &DatabaseConnection, interval: &str) -> usize {
    let chunks = db
        .query_all(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT format('%I.%I', chunk_schema, chunk_name) AS chunk \
             FROM timescaledb_information.chunks \
             WHERE hypertable_name = 'readings' AND NOT is_compressed \
               AND range_end <= now() - $1::interval \
             ORDER BY range_start",
            [interval.into()],
        ))
        .await
        .expect("readings chunk lookup failed");
    compress_each(db, chunks).await
}

/// Number of `readings` chunks currently compressed.
pub async fn compressed_readings_chunk_count(db: &DatabaseConnection) -> usize {
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT count(*)::bigint AS n FROM timescaledb_information.chunks \
             WHERE hypertable_name = 'readings' AND is_compressed",
        ))
        .await
        .expect("compressed chunk count failed")
        .expect("count returned no row");
    let n: i64 = row.try_get("", "n").expect("count column");
    usize::try_from(n).unwrap_or_default()
}

/// A second connection pool to `DATABASE_URL` whose every session caps decompression at `tuples`
/// rows per DML statement, so an UPDATE over a compressed chunk fails unless it lifts the cap itself.
pub async fn connect_with_decompression_cap(tuples: u32) -> DatabaseConnection {
    dotenvy::dotenv().ok();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    let separator = if url.contains('?') { '&' } else { '?' };
    let capped = format!(
        "{url}{separator}options=-c%20timescaledb.max_tuples_decompressed_per_dml_transaction%3D{tuples}"
    );
    Database::connect(&capped)
        .await
        .expect("failed to connect with a decompression cap")
}

async fn compress_each(db: &DatabaseConnection, chunks: Vec<QueryResult>) -> usize {
    let mut compressed = 0;
    for row in chunks {
        let chunk: String = row.try_get("", "chunk").expect("chunk name column");
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT compress_chunk($1::regclass, if_not_compressed => true)",
            [chunk.clone().into()],
        ))
        .await
        .unwrap_or_else(|e| panic!("compress_chunk({chunk}) failed: {e}"));
        compressed += 1;
    }
    compressed
}
