//! Pairing a stream that carries replicate-indexed readings (e.g. migrated NOMIS A/B/C rows)
//! groups them into samples: pair backfills site/parameter, find-or-creates one samples row per
//! replicate group, and stamps sample_id; unpair clears sample_id and removes the now
//! unreferenced samples.
//!
//! Run with: cargo test --test data_streams

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

const REPLICATE_TIME: &str = "2025-02-10T09:00:00Z";

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

async fn seed_stream_with_replicates(db: &DatabaseConnection) -> Uuid {
    let stream_id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', 'nomis', '{}', 'NOMIS lab column', true)",
            Uuid::new_v4()
        ),
    )
    .await;
    for (idx, value) in [(0, 5.0), (1, 6.0), (2, 7.0)] {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO readings (stream_id, time, replicate_index, raw_value, measurement_type) \
                 VALUES ('{stream_id}', '{REPLICATE_TIME}', {idx}, {value}, 'spot')"
            ),
        )
        .await;
    }
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, time, replicate_index, raw_value, measurement_type) \
             VALUES ('{stream_id}', '2025-02-10T10:00:00Z', 0, 9.0, 'spot')"
        ),
    )
    .await;
    stream_id
}

#[tokio::test]
#[serial]
async fn pairing_preserves_source_replicate_indexes() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let stream_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', 'nomis', '{}', 'NOMIS B/C/D only', true)",
            Uuid::new_v4()
        ),
    )
    .await;
    for (idx, value) in [(1, 5.0), (2, 6.0), (3, 7.0)] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, time, replicate_index, raw_value, measurement_type) \
                 VALUES ('{stream_id}', '{REPLICATE_TIME}', {idx}, {value}, 'spot')"
            ),
        )
        .await;
    }

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &serde_json::json!({ "site_parameter_id": crate::common::PARAM_S1_TEMP_ID }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "pair ({status}): {body}");

    let indices = scalar_i64(
        &db,
        &format!(
            "SELECT MIN(replicate_index)::bigint AS n FROM readings \
             WHERE stream_id = '{stream_id}' AND time = '{REPLICATE_TIME}'"
        ),
    )
    .await;
    assert_eq!(indices, 1, "the source's column positions are left alone");

    let sample_n = scalar_i64(
        &db,
        &format!(
            "SELECT n::bigint AS n FROM samples \
             WHERE site_id = '{}' AND parameter_id = '{}' AND collected_at = '{REPLICATE_TIME}'",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    assert_eq!(sample_n, 3, "all three replicates count toward the sample");
}

#[tokio::test]
#[serial]
async fn pair_groups_replicates_into_samples_and_unpair_clears_them() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let stream_id = seed_stream_with_replicates(&db).await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &serde_json::json!({ "site_parameter_id": crate::common::PARAM_S1_TEMP_ID }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "pair ({status}): {body}");

    let samples = scalar_i64(
        &db,
        &format!(
            "SELECT COUNT(*) AS n FROM samples \
             WHERE site_id = '{}' AND parameter_id = '{}' AND collected_at = '{REPLICATE_TIME}'",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    assert_eq!(samples, 1, "the replicate group formed one sample");

    let stamped = scalar_i64(
        &db,
        &format!(
            "SELECT COUNT(*) AS n FROM readings \
             WHERE stream_id = '{stream_id}' AND time = '{REPLICATE_TIME}' \
               AND sample_id IS NOT NULL"
        ),
    )
    .await;
    assert_eq!(stamped, 3, "all replicates reference the sample");

    let lone_stamped = scalar_i64(
        &db,
        &format!(
            "SELECT COUNT(*) AS n FROM readings \
             WHERE stream_id = '{stream_id}' AND time = '2025-02-10T10:00:00Z' \
               AND sample_id IS NOT NULL"
        ),
    )
    .await;
    assert_eq!(lone_stamped, 0, "a single reading does not get a sample");

    let sample_n = scalar_i64(
        &db,
        &format!(
            "SELECT n::bigint AS n FROM samples \
             WHERE site_id = '{}' AND parameter_id = '{}' AND collected_at = '{REPLICATE_TIME}'",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    assert_eq!(sample_n, 3, "the trigger populated the sample statistics");

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/unpair"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "unpair ({status}): {body}");

    let still_stamped = scalar_i64(
        &db,
        &format!(
            "SELECT COUNT(*) AS n FROM readings \
             WHERE stream_id = '{stream_id}' AND sample_id IS NOT NULL"
        ),
    )
    .await;
    assert_eq!(
        still_stamped, 0,
        "unpair clears sample_id on the stream's readings"
    );

    let samples_left = scalar_i64(
        &db,
        &format!(
            "SELECT COUNT(*) AS n FROM samples \
             WHERE site_id = '{}' AND parameter_id = '{}' AND collected_at = '{REPLICATE_TIME}'",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    assert_eq!(samples_left, 0, "the unreferenced sample is deleted");
}
