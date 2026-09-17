//! Memory a pairing's attribution backfill holds while it runs.
//!
//! Scenario: a stream already holding tens of thousands of readings is paired.
//! Expected behaviour: the executor's memory stays flat across the attribution `UPDATE`. TimescaleDB
//! keeps every row a hypertable `UPDATE ... RETURNING` emits in `ExecutorState` until the statement
//! ends, which at a few hundred thousand readings exhausted the database.

use sea_orm::{ConnectionTrait, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::sensor_lifecycle::create_unpaired_stream_with_device;
use crate::common::{PARAM_S1_TEMP_ID, exec};

const READINGS: u32 = 20_000;
const EXECUTOR_BOUND_BYTES: i64 = 16 * 1024 * 1024;

const PROBE_SETUP: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS executor_memory_probe (bytes bigint NOT NULL)",
    "CREATE OR REPLACE FUNCTION executor_memory_probe() RETURNS trigger LANGUAGE plpgsql AS $$
     BEGIN
         IF OLD.site_id IS NULL AND NEW.site_id IS NOT NULL AND random() < 0.005 THEN
             INSERT INTO executor_memory_probe
             SELECT sum(total_bytes) FROM pg_backend_memory_contexts WHERE name = 'ExecutorState';
         END IF;
         RETURN NEW;
     END $$",
    "CREATE TRIGGER zz_executor_memory_probe BEFORE UPDATE ON readings
     FOR EACH ROW EXECUTE FUNCTION executor_memory_probe()",
];

const PROBE_TEARDOWN: &[&str] = &[
    "DROP TRIGGER IF EXISTS zz_executor_memory_probe ON readings",
    "DROP FUNCTION IF EXISTS executor_memory_probe()",
    "DROP TABLE IF EXISTS executor_memory_probe",
];

async fn peak_executor_bytes(db: &sea_orm::DatabaseConnection) -> (i64, i64) {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COUNT(*)::bigint AS samples, COALESCE(MAX(bytes), 0)::bigint AS peak \
             FROM executor_memory_probe"
                .to_owned(),
        ))
        .await
        .expect("read the probe")
        .expect("one summary row");
    (
        row.try_get("", "samples").unwrap(),
        row.try_get("", "peak").unwrap(),
    )
}

#[tokio::test]
#[serial]
async fn test_pair_backfill_executor_memory_stays_flat() {
    let f = crate::common::seeded_app().await;
    let (app, token, db) = (f.app, f.token, f.db);

    let stream = create_unpaired_stream_with_device(&db, "backlog", "SB19-BACKLOG").await;
    exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, time, raw_value, replicate_index) \
             SELECT '{stream}', TIMESTAMPTZ '2024-01-01' + g * INTERVAL '10 minutes', g, 0 \
             FROM generate_series(1, {READINGS}) g"
        ),
    )
    .await;

    for sql in PROBE_TEARDOWN.iter().chain(PROBE_SETUP) {
        exec(&db, sql).await;
    }
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{stream}/pair"),
        &json!({ "site_parameter_id": PARAM_S1_TEMP_ID }),
        &token,
    )
    .await;
    let (samples, peak) = peak_executor_bytes(&db).await;
    for sql in PROBE_TEARDOWN {
        exec(&db, sql).await;
    }

    assert!((200..300).contains(&status), "pair ({status}): {body}");
    assert!(samples > 0, "the probe sampled the attribution update");
    // A leak of a few kilobytes per row is over 50 MB at this count.
    assert!(
        peak < EXECUTOR_BOUND_BYTES,
        "ExecutorState peaked at {peak} bytes over {READINGS} readings"
    );
}
