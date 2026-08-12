//! Scenario: something rewrites readings and then asks for the rollups covering them.
//!
//! Expected behaviour: `common::aggregates::refresh` materialises every bucket the window touches,
//! including the bucket the window starts in and the bucket now falls in, and reports a refresh it
//! could not run instead of logging it away.
//!
//! This suite owns 2026-07 so it materialises no bucket another refresh suite asserts on.
//!
//! Run: cargo test --test sites aggregate_refresh_windows -- --test-threads=1

use chrono::{DateTime, Duration, Utc};
use river_db::common::aggregates::{self, Window};
use sea_orm::{ConnectionTrait, Database, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;

use crate::common::sensor_lifecycle::seed_base_entities;
use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID, cleanup_test_db, exec, setup_test_db};

fn instant(ts: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(ts)
        .unwrap_or_else(|e| panic!("fixture time {ts} is not RFC 3339: {e}"))
        .with_timezone(&Utc)
}

async fn seed_slot(db: &DatabaseConnection) -> String {
    cleanup_test_db(db).await;
    seed_base_entities(db).await;
    let stream_id = uuid::Uuid::new_v4().to_string();
    exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', 'test', 'refresh_windows', 'Refresh windows', true)"
        ),
    )
    .await;
    stream_id
}

async fn add_reading(db: &DatabaseConnection, stream_id: &str, time: DateTime<Utc>, raw: f64) {
    exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{stream_id}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{}', {raw}, 0)",
            time.to_rfc3339()
        ),
    )
    .await;
}

/// `(bucket, count, avg)` rows of one rollup over a range, oldest first.
async fn buckets(
    db: &DatabaseConnection,
    view: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<(DateTime<Utc>, i64, f64)> {
    db.query_all(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        format!(
            "SELECT bucket, count::bigint AS n, avg_value::float8 AS avg FROM {view} \
             WHERE site_id = '{SITE1_ID}' AND bucket >= $1 AND bucket < $2 ORDER BY bucket"
        ),
        [from.into(), to.into()],
    ))
    .await
    .unwrap_or_else(|e| panic!("reading {view} failed: {e}"))
    .iter()
    .map(|r| {
        (
            r.try_get("", "bucket").expect("bucket column"),
            r.try_get("", "n").expect("count column"),
            r.try_get("", "avg").expect("avg column"),
        )
    })
    .collect()
}

#[tokio::test]
#[serial]
async fn the_bucket_the_window_starts_in_is_materialised() {
    let db = setup_test_db().await;
    let stream_id = seed_slot(&db).await;

    let first = instant("2026-07-08T14:22:00Z");
    let second = instant("2026-07-08T16:10:00Z");
    add_reading(&db, &stream_id, first, 10.0).await;
    add_reading(&db, &stream_id, second, 20.0).await;

    aggregates::refresh(&db, Window::Range(first, second))
        .await
        .expect("refresh over a historical range");

    let hourly = buckets(
        &db,
        "readings_hourly",
        instant("2026-07-08T00:00:00Z"),
        instant("2026-07-09T00:00:00Z"),
    )
    .await;
    assert_eq!(
        hourly.len(),
        2,
        "the 14:00 bucket holds the window start and must be materialised: {hourly:?}"
    );
    assert_eq!(hourly[0].0, instant("2026-07-08T14:00:00Z"));
    assert!((hourly[0].2 - 10.0).abs() < 1e-9);
    assert_eq!(hourly[1].0, instant("2026-07-08T16:00:00Z"));

    let daily = buckets(
        &db,
        "readings_daily",
        instant("2026-07-08T00:00:00Z"),
        instant("2026-07-09T00:00:00Z"),
    )
    .await;
    assert_eq!(daily.len(), 1, "{daily:?}");
    assert_eq!(daily[0].1, 2);
    assert!((daily[0].2 - 15.0).abs() < 1e-9);

    let weekly = buckets(
        &db,
        "readings_weekly",
        instant("2026-07-06T00:00:00Z"),
        instant("2026-07-13T00:00:00Z"),
    )
    .await;
    assert_eq!(weekly.len(), 1, "{weekly:?}");
    let monthly = buckets(
        &db,
        "readings_monthly",
        instant("2026-07-01T00:00:00Z"),
        instant("2026-08-01T00:00:00Z"),
    )
    .await;
    assert_eq!(monthly.len(), 1, "{monthly:?}");

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn a_window_narrower_than_a_bucket_still_refreshes() {
    let db = setup_test_db().await;
    let stream_id = seed_slot(&db).await;

    // A single reading a few minutes old: the raw window [t, now] is a fraction of every bucket.
    let recent = Utc::now() - Duration::minutes(3);
    add_reading(&db, &stream_id, recent, 42.0).await;

    aggregates::refresh(&db, Window::Since(recent))
        .await
        .expect("a sub-bucket window must not raise 'refresh window too small'");

    let hourly = buckets(
        &db,
        "readings_hourly",
        recent - Duration::hours(2),
        recent + Duration::hours(2),
    )
    .await;
    assert_eq!(
        hourly.len(),
        1,
        "the current, incomplete bucket must be materialised: {hourly:?}"
    );
    assert!((hourly[0].2 - 42.0).abs() < 1e-9);

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn a_refresh_that_cannot_run_returns_the_error() {
    // A database with no rollups at all: the CALL fails and the caller must hear about it rather
    // than reading a warn line in the log.
    dotenvy::dotenv().ok();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    let Some((prefix, _)) = url.rsplit_once('/') else {
        panic!("DATABASE_URL has no database segment: {url}");
    };
    let maintenance = Database::connect(format!("{prefix}/postgres"))
        .await
        .expect("connect to the maintenance database");

    let outcome =
        aggregates::refresh(&maintenance, Window::Since(Utc::now() - Duration::days(1))).await;
    assert!(
        outcome.is_err(),
        "a failed continuous-aggregate refresh must surface as an error"
    );
}
