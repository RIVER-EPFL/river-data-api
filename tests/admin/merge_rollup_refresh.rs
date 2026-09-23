//! Scenario: a parameter or slot merge moves a continuous reading, and the rollup refresh over the
//! span it moved cannot be queued, or the refresh itself fails.
//!
//! Expected behaviour: the refresh is a `refresh_aggregates` job queued with the merge, so a
//! refused enqueue merges nothing and a retry finds its source still there, and a refresh that
//! fails is the job's failure, not the merge's.
//!
//! Run: cargo test --test admin merge_rollup_refresh -- --test-threads=1

use river_db::routes::private::parameters::service::{MergeParametersRequest, merge_parameters};
use river_db::routes::private::readings::models::Origin;
use river_db::routes::private::site_parameters::service::{
    MergeSiteParametersRequest, merge_site_parameters,
};
use sea_orm::DatabaseConnection;
use serial_test::serial;

use crate::common::jobs::{refuse_enqueue, refuse_refresh, restore_enqueue, restore_refresh};
use crate::common::sensor_lifecycle::{create_paired_stream, seed_base_entities};
use crate::common::*;

const AT: &str = "2025-10-06T09:00:00Z";

/// One continuous temperature reading, which the merges move into dissolved oxygen.
async fn setup() -> DatabaseConnection {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    let stream = create_paired_stream(&db, "merge-rollup", PARAM_S1_TEMP_ID).await;
    exec(
        &db,
        &format!(
            "INSERT INTO readings \
                 (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{stream}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{AT}', 10, 0)"
        ),
    )
    .await;
    db
}

fn parameters() -> MergeParametersRequest {
    MergeParametersRequest {
        source_parameter_id: GLOBAL_PARAM_TEMP_ID.parse().unwrap(),
        target_parameter_id: GLOBAL_PARAM_DO_ID.parse().unwrap(),
    }
}

fn slots() -> MergeSiteParametersRequest {
    MergeSiteParametersRequest {
        source_site_parameter_id: PARAM_S1_TEMP_ID.parse().unwrap(),
        target_site_parameter_id: PARAM_S1_DO_ID.parse().unwrap(),
    }
}

async fn moved(db: &DatabaseConnection) -> i64 {
    e2e::count(
        db,
        &format!(
            "SELECT COUNT(*)::bigint FROM readings WHERE parameter_id = '{GLOBAL_PARAM_DO_ID}'"
        ),
    )
    .await
}

/// The `refresh_aggregates` jobs queued over exactly the moved instant.
async fn refreshes_queued(db: &DatabaseConnection) -> i64 {
    e2e::count(
        db,
        &format!(
            "SELECT COUNT(*)::bigint FROM reprocessing_jobs \
             WHERE trigger_type = 'refresh_aggregates' \
               AND (params->>'from')::timestamptz = '{AT}' \
               AND (params->>'until')::timestamptz = '{AT}'"
        ),
    )
    .await
}

#[tokio::test]
#[serial]
async fn a_parameter_merge_whose_rollup_refresh_cannot_be_queued_merges_nothing() {
    let db = setup().await;

    refuse_enqueue(&db, "refresh_aggregates").await;
    let refused = merge_parameters(&db, &parameters(), "tester", Origin::Manual, None).await;
    restore_enqueue(&db).await;

    assert!(refused.is_err(), "the merge reports the failure");
    assert_eq!(moved(&db).await, 0, "no reading moved");
    merge_parameters(&db, &parameters(), "tester", Origin::Manual, None)
        .await
        .expect("a retry merges the parameter");
    assert_eq!(moved(&db).await, 1);
    assert_eq!(refreshes_queued(&db).await, 1);
}

#[tokio::test]
#[serial]
async fn a_slot_merge_whose_rollup_refresh_cannot_be_queued_merges_nothing() {
    let db = setup().await;

    refuse_enqueue(&db, "refresh_aggregates").await;
    let refused = merge_site_parameters(&db, &slots(), "tester", Origin::Manual, None).await;
    restore_enqueue(&db).await;

    assert!(refused.is_err(), "the merge reports the failure");
    assert_eq!(moved(&db).await, 0, "no reading moved");
    merge_site_parameters(&db, &slots(), "tester", Origin::Manual, None)
        .await
        .expect("a retry merges the slot");
    assert_eq!(moved(&db).await, 1);
    assert_eq!(refreshes_queued(&db).await, 1);
}

#[tokio::test]
#[serial]
async fn a_slot_merge_whose_rollup_refresh_fails_reports_the_merge_it_made() {
    let db = setup().await;

    refuse_refresh(&db).await;
    let merged = merge_site_parameters(&db, &slots(), "tester", Origin::Manual, None).await;
    restore_refresh(&db).await;

    assert!(merged.is_ok(), "the merge is committed: {merged:?}");
    assert_eq!(moved(&db).await, 1);
}
