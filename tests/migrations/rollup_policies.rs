//! Scenario: a migration recreates a rollup `WITH NO DATA` and re-adds its refresh policy.
//!
//! Expected behaviour: the four rollups serve their open bucket from the raw rows, and each policy
//! covers the whole history in one batch, so a rollup left empty is filled by its first run rather
//! than one bucket per tick. Nothing else notices either state: a materialized-only rollup serves
//! an unmaterialised bucket as absence, and a batching policy on an empty monthly rollup never
//! converges at all.
//!
//! Its own database, because `cleanup_test_db` removes these policies from the shared one.
//!
//! Run: cargo test --test migrations rollup_policies -- --test-threads=1

use chrono::{Duration, Utc};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

use crate::common::scratch;
use crate::common::sensor_lifecycle::seed_base_entities;
use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

/// The rollups and the bucket each policy stops one of before now.
const VIEWS: [(&str, &str); 4] = [
    ("readings_hourly", "01:00:00"),
    ("readings_daily", "1 day"),
    ("readings_weekly", "7 days"),
    ("readings_monthly", "1 mon"),
];

async fn exec(db: &DatabaseConnection, sql: &str) {
    db.execute_unprepared(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql} failed: {e}"));
}

async fn scalar<T>(db: &DatabaseConnection, sql: &str) -> T
where
    T: sea_orm::TryGetable,
{
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("{sql} failed: {e}"))
    .unwrap_or_else(|| panic!("{sql} returned no row"))
    .try_get::<T>("", "v")
    .unwrap_or_else(|e| panic!("{sql} has no column v: {e}"))
}

/// The rows `readings_hourly` serves for the fixture site, counted from the rollup itself.
async fn hourly_buckets(db: &DatabaseConnection) -> i64 {
    scalar(
        db,
        &format!("SELECT count(*)::bigint AS v FROM readings_hourly WHERE site_id = '{SITE1_ID}'"),
    )
    .await
}

/// Write `raw` at `hours_ago`, on a stream of this suite's own.
async fn write_reading(db: &DatabaseConnection, stream_id: &str, minutes_ago: i64, raw: f64) {
    let time = (Utc::now() - Duration::minutes(minutes_ago)).to_rfc3339();
    exec(
        db,
        &format!(
            "INSERT INTO readings \
               (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{stream_id}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{time}', {raw}, 0)"
        ),
    )
    .await;
}

#[tokio::test]
#[serial]
async fn the_rollups_serve_their_open_bucket_and_fill_themselves_from_empty() {
    dotenvy::dotenv().ok();
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    let name = format!("river_rollups_{}", std::process::id());
    let server = scratch::server(&base).await;
    let db = scratch::build(&base, &server, &name).await;

    let outcome = tokio::spawn({
        let db = db.clone();
        async move { assertions(&db).await }
    })
    .await;

    db.close().await.expect("close the scratch connection");
    scratch::discard(&server, &name).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn assertions(db: &DatabaseConnection) {
    // --- The rollups are real-time ---
    let views: Vec<(String, bool)> = db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT view_name, materialized_only \
               FROM timescaledb_information.continuous_aggregates \
              WHERE view_name LIKE 'readings\\_%' ORDER BY view_name"
                .to_string(),
        ))
        .await
        .expect("listing the continuous aggregates")
        .iter()
        .map(|r| {
            (
                r.try_get("", "view_name").expect("view_name"),
                r.try_get("", "materialized_only")
                    .expect("materialized_only"),
            )
        })
        .collect();
    assert_eq!(views.len(), 4, "the four rollups are present: {views:?}");
    assert!(
        views.iter().all(|(_, only)| !*only),
        "a materialized-only rollup serves its open bucket as absence, which is the head of \
         every series: {views:?}"
    );

    // --- Each policy covers the whole history, in one batch ---
    for (view, bucket) in VIEWS {
        let config = policy(db, view).await;
        assert_eq!(
            config.get("start_offset"),
            Some(&serde_json::Value::Null),
            "{view}'s policy must cover the whole history, so a correction to an old reading is \
             healed by the next tick: {config}"
        );
        assert_eq!(
            config.get("buckets_per_batch").and_then(|v| v.as_i64()),
            Some(0),
            "{view}'s policy must refresh its window in one batch; batching never converges on \
             an empty rollup: {config}"
        );
        assert_eq!(
            config.get("end_offset").and_then(|v| v.as_str()),
            Some(bucket),
            "{view}'s policy stops one bucket before now, and the open bucket is served raw: \
             {config}"
        );
    }

    // --- The open bucket is served without any refresh ---
    seed_base_entities(db).await;
    // Every channel carries an instrument: a reading may not be stored without naming what
    // measured it, and the insert trigger stamps one from the stream.
    let sensor_id = uuid::Uuid::new_v4();
    let stream_id = uuid::Uuid::new_v4();
    exec(
        db,
        &format!(
            "INSERT INTO sensors (id, name) VALUES ('{sensor_id}', 'Rollup policies');
             INSERT INTO data_streams \
               (id, source_system, source_key, source_name, sensor_id, is_active) \
             VALUES ('{stream_id}', 'test', 'rollup_policies', 'Rollup policies', \
                     '{sensor_id}', true)"
        ),
    )
    .await;
    let stream_id = stream_id.to_string();

    write_reading(db, &stream_id, 1, 7.5).await;
    assert_eq!(
        hourly_buckets(db).await,
        1,
        "a reading in the bucket the current hour is still filling is served from the raw rows, \
         with nothing materialised yet"
    );

    // --- An empty rollup fills on its first policy run ---
    // Three hours of history, each in a bucket the policy's window already covers.
    for hours in 2..5 {
        write_reading(db, &stream_id, hours * 60, 7.5).await;
    }
    exec(db, "TRUNCATE readings_hourly").await;
    let hourly = policy(db, "readings_hourly").await;
    let mat_id = hourly
        .get("mat_hypertable_id")
        .and_then(serde_json::Value::as_i64)
        .expect("the policy names the materialization hypertable");
    let materialised = format!(
        "SELECT count(*)::bigint AS v \
           FROM _timescaledb_internal._materialized_hypertable_{mat_id} \
          WHERE site_id = '{SITE1_ID}'"
    );
    assert_eq!(
        scalar::<i64>(db, &materialised).await,
        0,
        "the rollup starts this half of the story empty"
    );

    let job_id: i32 = scalar(
        db,
        "SELECT job_id AS v FROM timescaledb_information.jobs \
          WHERE proc_name = 'policy_refresh_continuous_aggregate' \
            AND hypertable_name = 'readings_hourly'",
    )
    .await;
    exec(db, &format!("CALL run_job({job_id})")).await;

    let materialised: i64 = scalar(db, &materialised).await;
    assert_eq!(
        materialised, 3,
        "one policy run fills the whole history it covers, not one bucket of it"
    );
    assert_eq!(
        hourly_buckets(db).await,
        4,
        "and the rollup serves the materialised history together with the open bucket"
    );
}

/// A rollup's refresh policy, as TimescaleDB holds it. The view is what the job names.
async fn policy(db: &DatabaseConnection, view: &str) -> serde_json::Value {
    scalar(
        db,
        &format!(
            "SELECT config AS v FROM timescaledb_information.jobs \
              WHERE proc_name = 'policy_refresh_continuous_aggregate' \
                AND hypertable_name = '{view}'"
        ),
    )
    .await
}
