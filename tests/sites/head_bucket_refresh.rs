//! Scenario: a reading lands in the bucket the current hour is still filling.
//!
//! Expected behaviour: every rollup is `materialized_only`, so the newest bucket is served as
//! absence until something materialises it, and the scheduled `Window::Recent` refresh is what
//! does. Deleting that refresh as redundant with TimescaleDB's own policies silently drops the
//! head of every series from `readings_hourly`, because every policy carries an `end_offset` that
//! leaves the newest bucket alone.
//!
//! Run: cargo test --test sites head_bucket_refresh -- --test-threads=1

use chrono::{Duration, Utc};
use river_db::common::aggregates::{self, Window};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

use crate::common::sensor_lifecycle::seed_base_entities;
use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID, cleanup_test_db, exec, setup_test_db};

async fn head_bucket_count(db: &DatabaseConnection) -> i64 {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT coalesce(sum(count), 0)::bigint AS n FROM readings_hourly \
             WHERE site_id = '{SITE1_ID}' AND bucket >= date_trunc('hour', now())"
        ),
    ))
    .await
    .expect("reading readings_hourly failed")
    .expect("one row")
    .try_get("", "n")
    .expect("count column")
}

#[tokio::test]
#[serial]
async fn the_newest_bucket_reaches_the_rollup_only_through_a_recent_refresh() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let views: Vec<(String, bool)> = db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT view_name, materialized_only FROM timescaledb_information.continuous_aggregates \
             WHERE view_name LIKE 'readings_%' ORDER BY view_name",
        ))
        .await
        .expect("listing the continuous aggregates failed")
        .iter()
        .map(|r| {
            (
                r.try_get("", "view_name").expect("view_name column"),
                r.try_get("", "materialized_only")
                    .expect("materialized_only column"),
            )
        })
        .collect();
    assert_eq!(
        views.len(),
        4,
        "the four rollups must all be present: {views:?}"
    );
    assert!(
        views.iter().all(|(_, only)| *only),
        "a materialized-only rollup serves an unmaterialised bucket as absence, which is what \
         the scheduled refresh exists to fill: {views:?}"
    );

    let stream_id = uuid::Uuid::new_v4().to_string();
    exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', 'test', 'head_bucket', 'Head bucket', true)"
        ),
    )
    .await;
    let now = Utc::now();
    exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{stream_id}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{}', 7.5, 0)",
            (now - Duration::minutes(1)).to_rfc3339()
        ),
    )
    .await;

    assert_eq!(
        head_bucket_count(&db).await,
        0,
        "the bucket the current hour is still filling holds nothing until a refresh runs"
    );

    aggregates::refresh(&db, Window::Recent)
        .await
        .expect("the scheduled rolling refresh");

    assert_eq!(
        head_bucket_count(&db).await,
        1,
        "the head of the series must reach readings_hourly through the Recent refresh"
    );

    cleanup_test_db(&db).await;
}
