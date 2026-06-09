//! Integration tests for the `samples` entity and its aggregate-maintenance trigger.
//!
//! Run with: cargo test --test samples
//! Requires: DATABASE_URL pointing to a TimescaleDB instance.


use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

struct SampleAggregate {
    mean: Option<f64>,
    stdev: Option<f64>,
    n: i32,
    min_value: Option<f64>,
    max_value: Option<f64>,
}

async fn fetch_aggregate(db: &DatabaseConnection, sample_id: Uuid) -> SampleAggregate {
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT mean, stdev, n, min_value, max_value \
                 FROM samples WHERE id = '{sample_id}'"
            ),
        ))
        .await
        .expect("query samples")
        .expect("samples row exists");

    SampleAggregate {
        mean: row.try_get::<Option<f64>>("", "mean").unwrap(),
        stdev: row.try_get::<Option<f64>>("", "stdev").unwrap(),
        n: row.try_get::<i32>("", "n").unwrap(),
        min_value: row.try_get::<Option<f64>>("", "min_value").unwrap(),
        max_value: row.try_get::<Option<f64>>("", "max_value").unwrap(),
    }
}

async fn exec(db: &DatabaseConnection, sql: &str) {
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("SQL failed: {e}\nQuery: {sql}"));
}

/// Ensure a "grab_sample" data stream exists and return its id for a
/// (site, parameter) pair. Mirrors the helper used by the grab_samples handler.
async fn ensure_stream(
    db: &DatabaseConnection,
    site_id: &str,
    parameter_id: &str,
) -> Uuid {
    let source_key = format!("{site_id}:{parameter_id}");
    let stream_id = Uuid::new_v4();
    exec(
        db,
        &format!(
            "INSERT INTO data_streams \
             (id, source_system, source_key, metadata, is_active, discovered_at, created_at, updated_at) \
             VALUES ('{stream_id}', 'grab_sample', '{source_key}', '{{}}', true, NOW(), NOW(), NOW()) \
             ON CONFLICT (source_system, source_key) DO NOTHING"
        ),
    )
    .await;

    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT id FROM data_streams \
                 WHERE source_system = 'grab_sample' AND source_key = '{source_key}'"
            ),
        ))
        .await
        .expect("query data_streams")
        .expect("data_stream exists");

    row.try_get::<Uuid>("", "id").unwrap()
}

async fn create_sample(
    db: &DatabaseConnection,
    site_id: &str,
    parameter_id: &str,
    label: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    exec(
        db,
        &format!(
            "INSERT INTO samples (id, site_id, parameter_id, collected_at, label) \
             VALUES ('{id}', '{site_id}', '{parameter_id}', '2025-01-15T12:00:00Z', '{label}')"
        ),
    )
    .await;
    id
}

async fn insert_replicate(
    db: &DatabaseConnection,
    stream_id: Uuid,
    site_id: &str,
    parameter_id: &str,
    sample_id: Uuid,
    replicate_index: i16,
    value: f64,
) {
    exec(
        db,
        &format!(
            "INSERT INTO readings \
             (stream_id, time, replicate_index, site_id, parameter_id, \
              raw_value, calibrated_value, sample_id, is_flagged) \
             VALUES ('{stream_id}', '2025-01-15T12:00:00Z', {replicate_index}, \
                     '{site_id}', '{parameter_id}', {value}, {value}, '{sample_id}', false)"
        ),
    )
    .await;
}

fn mean(vs: &[f64]) -> f64 {
    vs.iter().sum::<f64>() / (vs.len() as f64)
}

fn stdev_sample(vs: &[f64]) -> f64 {
    let m = mean(vs);
    let n = vs.len();
    if n < 2 {
        return 0.0;
    }
    let variance = vs.iter().map(|v| (v - m).powi(2)).sum::<f64>() / ((n - 1) as f64);
    variance.sqrt()
}

/// Happy path: create a sample with 3 replicates → aggregate columns match reference.
#[tokio::test]
#[serial]
async fn samples_trigger_populates_aggregate_on_insert() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    let stream_id = ensure_stream(&db, crate::common::SITE1_ID, crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let sample_id =
        create_sample(&db, crate::common::SITE1_ID, crate::common::GLOBAL_PARAM_TEMP_ID, "test-happy").await;

    let values = [10.0_f64, 12.0, 14.0];
    for (i, v) in values.iter().enumerate() {
        insert_replicate(
            &db,
            stream_id,
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID,
            sample_id,
            (i as i16) + 1,
            *v,
        )
        .await;
    }

    let agg = fetch_aggregate(&db, sample_id).await;
    assert_eq!(agg.n, 3);
    assert!((agg.mean.unwrap() - mean(&values)).abs() < 1e-9);
    assert!((agg.stdev.unwrap() - stdev_sample(&values)).abs() < 1e-9);
    assert_eq!(agg.min_value.unwrap(), 10.0);
    assert_eq!(agg.max_value.unwrap(), 14.0);
}

/// Flag one replicate → aggregate excludes it on the next read.
#[tokio::test]
#[serial]
async fn samples_trigger_recomputes_on_flag() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    let stream_id = ensure_stream(&db, crate::common::SITE1_ID, crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let sample_id =
        create_sample(&db, crate::common::SITE1_ID, crate::common::GLOBAL_PARAM_TEMP_ID, "test-flag").await;

    for (i, v) in [10.0_f64, 12.0, 14.0].iter().enumerate() {
        insert_replicate(
            &db,
            stream_id,
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID,
            sample_id,
            (i as i16) + 1,
            *v,
        )
        .await;
    }

    exec(
        &db,
        &format!(
            "UPDATE readings SET is_flagged = true \
             WHERE sample_id = '{sample_id}' AND replicate_index = 2"
        ),
    )
    .await;

    let agg = fetch_aggregate(&db, sample_id).await;
    assert_eq!(agg.n, 2);
    assert!((agg.mean.unwrap() - 12.0).abs() < 1e-9);
}

/// Delete all replicates → sample persists with n=0 and NULL aggregates.
#[tokio::test]
#[serial]
async fn samples_trigger_handles_all_deleted() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    let stream_id = ensure_stream(&db, crate::common::SITE1_ID, crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let sample_id =
        create_sample(&db, crate::common::SITE1_ID, crate::common::GLOBAL_PARAM_TEMP_ID, "test-empty").await;

    insert_replicate(
        &db,
        stream_id,
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        sample_id,
        1,
        7.0,
    )
    .await;

    let agg = fetch_aggregate(&db, sample_id).await;
    assert_eq!(agg.n, 1);

    exec(
        &db,
        &format!("DELETE FROM readings WHERE sample_id = '{sample_id}'"),
    )
    .await;

    let agg = fetch_aggregate(&db, sample_id).await;
    assert_eq!(agg.n, 0);
    assert!(agg.mean.is_none());
    assert!(agg.stdev.is_none());
    assert!(agg.min_value.is_none());
    assert!(agg.max_value.is_none());
}

/// Reassign a reading's sample_id: both old and new samples recompute.
#[tokio::test]
#[serial]
async fn samples_trigger_handles_reassignment() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    let stream_id = ensure_stream(&db, crate::common::SITE1_ID, crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let sample_a =
        create_sample(&db, crate::common::SITE1_ID, crate::common::GLOBAL_PARAM_TEMP_ID, "a").await;
    // Create sample B at a different timestamp so its readings don't collide
    // with sample A's (stream_id, time, replicate_index) PK.
    let sample_b_id = Uuid::new_v4();
    exec(
        &db,
        &format!(
            "INSERT INTO samples (id, site_id, parameter_id, collected_at, label) \
             VALUES ('{sample_b_id}', '{}', '{}', '2025-01-15T13:00:00Z', 'b')",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;

    // Two replicates in A
    insert_replicate(
        &db,
        stream_id,
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        sample_a,
        1,
        10.0,
    )
    .await;
    insert_replicate(
        &db,
        stream_id,
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        sample_a,
        2,
        20.0,
    )
    .await;

    // One replicate in B at time +1h
    exec(
        &db,
        &format!(
            "INSERT INTO readings \
             (stream_id, time, replicate_index, site_id, parameter_id, \
              raw_value, calibrated_value, sample_id, is_flagged) \
             VALUES ('{stream_id}', '2025-01-15T13:00:00Z', 1, \
                     '{}', '{}', 30.0, 30.0, '{sample_b_id}', false)",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;

    assert_eq!(fetch_aggregate(&db, sample_a).await.n, 2);
    assert_eq!(fetch_aggregate(&db, sample_b_id).await.n, 1);

    // Move replicate_index=2 from A to B (note: it stays at time 12:00, so PK is fine in B too)
    exec(
        &db,
        &format!(
            "UPDATE readings SET sample_id = '{sample_b_id}' \
             WHERE sample_id = '{sample_a}' AND replicate_index = 2"
        ),
    )
    .await;

    let agg_a = fetch_aggregate(&db, sample_a).await;
    let agg_b = fetch_aggregate(&db, sample_b_id).await;
    assert_eq!(agg_a.n, 1, "sample A should have recomputed to n=1");
    assert_eq!(agg_b.n, 2, "sample B should have recomputed to n=2");
    assert!((agg_a.mean.unwrap() - 10.0).abs() < 1e-9);
    assert!((agg_b.mean.unwrap() - 25.0).abs() < 1e-9);
}

/// Deleting the sample leaves readings orphaned (sample_id = NULL).
#[tokio::test]
#[serial]
async fn samples_delete_sets_reading_sample_id_null() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    let stream_id = ensure_stream(&db, crate::common::SITE1_ID, crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let sample_id =
        create_sample(&db, crate::common::SITE1_ID, crate::common::GLOBAL_PARAM_TEMP_ID, "del").await;

    insert_replicate(
        &db,
        stream_id,
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        sample_id,
        1,
        7.0,
    )
    .await;

    exec(&db, &format!("DELETE FROM samples WHERE id = '{sample_id}'")).await;

    // The reading should still exist, with sample_id = NULL.
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT sample_id::text, raw_value FROM readings \
                 WHERE stream_id = '{stream_id}' \
                   AND time = '2025-01-15T12:00:00Z' \
                   AND replicate_index = 1"
            ),
        ))
        .await
        .expect("query reading")
        .expect("reading persists");

    let sample_id_opt: Option<String> = row.try_get("", "sample_id").unwrap();
    assert!(sample_id_opt.is_none(), "sample_id should be NULL after parent delete");
    let raw_value: f64 = row.try_get("", "raw_value").unwrap();
    assert!((raw_value - 7.0).abs() < 1e-9);
}
