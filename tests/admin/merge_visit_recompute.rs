//! Scenario: a parameter or slot merge moves a spot value at a manual visit out of or into a
//! parameter a calculation reads, and the visit's `event_recompute` cannot be queued.
//!
//! Expected behaviour: the recompute is queued with the merge, whichever side the calculation
//! reads, so a refused enqueue merges nothing and a retry finds its source still there.
//!
//! Run: cargo test --test admin merge_visit_recompute -- --test-threads=1

use river_db::routes::private::parameters::service::{MergeParametersRequest, merge_parameters};
use river_db::routes::private::readings::models::Origin;
use river_db::routes::private::site_parameters::service::{
    MergeSiteParametersRequest, merge_site_parameters,
};
use sea_orm::DatabaseConnection;
use serial_test::serial;
use uuid::Uuid;

use crate::common::sensor_lifecycle::{create_paired_stream, seed_base_entities};
use crate::common::*;

const AT: &str = "2025-10-06T09:00:00Z";

/// A manual visit holding one spot temperature reading, which the merges move into dissolved
/// oxygen, and a calculation reading `read_code` there. Returns the visit.
async fn setup(read_code: &str) -> (DatabaseConnection, Uuid) {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    seed_visit_calculation(&db, "merge_visit_input", read_code).await;
    let stream = create_paired_stream(&db, "merge-visit", PARAM_S1_TEMP_ID).await;
    let event = Uuid::new_v4();
    exec(
        &db,
        &format!(
            "INSERT INTO collection_events (id, site_id, collected_at, source) \
             VALUES ('{event}', '{SITE1_ID}', '{AT}', 'manual')"
        ),
    )
    .await;
    exec(
        &db,
        &format!(
            "INSERT INTO readings \
                 (stream_id, site_id, parameter_id, time, raw_value, replicate_index, \
                  measurement_type, collection_event_id) \
             VALUES ('{stream}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{AT}', 10, 0, 'spot', \
                     '{event}')"
        ),
    )
    .await;
    (db, event)
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

async fn rows(db: &DatabaseConnection, sql: &str) -> i64 {
    e2e::count(db, sql).await
}

async fn moved(db: &DatabaseConnection) -> i64 {
    rows(
        db,
        &format!(
            "SELECT COUNT(*)::bigint FROM readings WHERE parameter_id = '{GLOBAL_PARAM_DO_ID}'"
        ),
    )
    .await
}

async fn recomputes_queued(db: &DatabaseConnection, event: Uuid) -> i64 {
    rows(
        db,
        &format!(
            "SELECT COUNT(*)::bigint FROM reprocessing_jobs \
             WHERE trigger_type = 'event_recompute' AND trigger_id = '{event}'"
        ),
    )
    .await
}

#[tokio::test]
#[serial]
async fn a_parameter_merge_whose_visit_recompute_cannot_be_queued_merges_nothing() {
    let (db, event) = setup("DO_Temperature").await;

    crate::common::jobs::refuse_enqueue(&db, "event_recompute").await;
    let refused = merge_parameters(&db, &parameters(), "tester", Origin::Manual, None).await;
    crate::common::jobs::restore_enqueue(&db).await;

    assert!(refused.is_err(), "the merge reports the failure");
    assert_eq!(moved(&db).await, 0, "no reading moved");
    assert_eq!(
        rows(
            &db,
            &format!("SELECT COUNT(*)::bigint FROM parameters WHERE id = '{GLOBAL_PARAM_TEMP_ID}'"),
        )
        .await,
        1,
        "the source parameter is still there"
    );
    merge_parameters(&db, &parameters(), "tester", Origin::Manual, None)
        .await
        .expect("a retry merges the parameter");
    assert_eq!(moved(&db).await, 1);
    assert_eq!(recomputes_queued(&db, event).await, 1);
}

#[tokio::test]
#[serial]
async fn a_slot_merge_whose_visit_recompute_cannot_be_queued_merges_nothing() {
    let (db, event) = setup("DO_Temperature").await;

    crate::common::jobs::refuse_enqueue(&db, "event_recompute").await;
    let refused = merge_site_parameters(&db, &slots(), "tester", Origin::Manual, None).await;
    crate::common::jobs::restore_enqueue(&db).await;

    assert!(refused.is_err(), "the merge reports the failure");
    assert_eq!(moved(&db).await, 0, "no reading moved");
    assert_eq!(
        rows(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint FROM site_parameters WHERE id = '{PARAM_S1_TEMP_ID}'"
            ),
        )
        .await,
        1,
        "the source slot is still there"
    );
    merge_site_parameters(&db, &slots(), "tester", Origin::Manual, None)
        .await
        .expect("a retry merges the slot");
    assert_eq!(moved(&db).await, 1);
    assert_eq!(recomputes_queued(&db, event).await, 1);
}

#[tokio::test]
#[serial]
async fn a_parameter_merge_queues_the_recompute_of_a_calculation_reading_the_target() {
    let (db, event) = setup("Dissolved_O2").await;

    merge_parameters(&db, &parameters(), "tester", Origin::Manual, None)
        .await
        .expect("the merge commits");

    assert_eq!(moved(&db).await, 1);
    assert_eq!(recomputes_queued(&db, event).await, 1);
}

#[tokio::test]
#[serial]
async fn a_slot_merge_queues_the_recompute_of_a_calculation_reading_the_target() {
    let (db, event) = setup("Dissolved_O2").await;

    merge_site_parameters(&db, &slots(), "tester", Origin::Manual, None)
        .await
        .expect("the merge commits");

    assert_eq!(moved(&db).await, 1);
    assert_eq!(recomputes_queued(&db, event).await, 1);
}
