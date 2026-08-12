//! Scenario: two slots at one site describe the same measurement and an operator merges them.
//!
//! Expected behaviour: every table keyed by (site, parameter) travels to the survivor, not only the
//! two the merge used to know about; and a move that would collide on a survivor's unique
//! constraint is refused whole rather than half-applied.
//!
//! Run: cargo test --test admin slot_keyed_merge -- --test-threads=1

use river_db::error::AppError;
use river_db::routes::private::admin::merge_services::{
    MergeSiteParametersRequest, merge_site_parameters,
};
use serial_test::serial;
use uuid::Uuid;

use crate::common::sensor_lifecycle::{create_paired_stream, seed_base_entities};
use crate::common::*;

const DAY: &str = "2025-11-18";

fn request() -> MergeSiteParametersRequest {
    MergeSiteParametersRequest {
        source_site_parameter_id: PARAM_S1_TEMP_ID.parse().unwrap(),
        target_site_parameter_id: PARAM_S1_DO_ID.parse().unwrap(),
    }
}

async fn add_sample(
    db: &sea_orm::DatabaseConnection,
    parameter_id: &str,
    at: &str,
    stream: Uuid,
    value: f64,
) -> Uuid {
    let sample_id = Uuid::new_v4();
    exec(
        db,
        &format!(
            "INSERT INTO samples (id, site_id, parameter_id, collected_at) \
             VALUES ('{sample_id}', '{SITE1_ID}', '{parameter_id}', '{at}')"
        ),
    )
    .await;
    exec(
        db,
        &format!(
            "INSERT INTO readings \
             (stream_id, site_id, parameter_id, time, raw_value, calibrated_value, \
              replicate_index, sample_id, measurement_type) \
             VALUES ('{stream}', '{SITE1_ID}', '{parameter_id}', '{at}', {value}, {value}, 0, \
                     '{sample_id}', 'spot')"
        ),
    )
    .await;
    sample_id
}

async fn add_annotation(db: &sea_orm::DatabaseConnection, parameter_id: &str) {
    exec(
        db,
        &format!(
            "INSERT INTO annotations (site_id, parameter_id, start_time, end_time, text) \
             VALUES ('{SITE1_ID}', '{parameter_id}', '{DAY}T00:00:00Z', '{DAY}T23:59:59Z', \
                     'bottle handled warm')"
        ),
    )
    .await;
}

async fn rows_for(db: &sea_orm::DatabaseConnection, table: &str, parameter_id: &str) -> i64 {
    e2e::count(
        db,
        &format!("SELECT COUNT(*)::bigint FROM {table} WHERE parameter_id = '{parameter_id}'"),
    )
    .await
}

#[tokio::test]
#[serial]
async fn a_merge_carries_every_slot_keyed_table_to_the_survivor() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let stream = create_paired_stream(&db, "merge-slot-tables", PARAM_S1_TEMP_ID).await;
    exec(
        &db,
        &format!(
            "INSERT INTO readings \
             (stream_id, site_id, parameter_id, time, raw_value, calibrated_value, replicate_index) \
             VALUES ('{stream}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{DAY}T11:00:00Z', \
                     300.0, 300.0, 0)"
        ),
    )
    .await;
    exec(
        &db,
        &format!(
            "INSERT INTO status_events (stream_id, site_id, parameter_id, time, value) \
             VALUES ('{stream}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{DAY}T11:05:00Z', \
                     'offline')"
        ),
    )
    .await;
    add_sample(
        &db,
        GLOBAL_PARAM_TEMP_ID,
        &format!("{DAY}T09:00:00Z"),
        stream,
        310.0,
    )
    .await;
    add_annotation(&db, GLOBAL_PARAM_TEMP_ID).await;

    let result = merge_site_parameters(&db, &request())
        .await
        .expect("the merge applies");
    assert!(result.source_deleted);
    assert_eq!(result.merged_readings, 2, "the grab replicate moves too");
    assert_eq!(result.merged_status_events, 1);

    for table in ["readings", "status_events", "samples", "annotations"] {
        assert_eq!(
            rows_for(&db, table, GLOBAL_PARAM_TEMP_ID).await,
            0,
            "{table} left nothing on the absorbed parameter"
        );
        assert!(
            rows_for(&db, table, GLOBAL_PARAM_DO_ID).await > 0,
            "{table} travelled to the survivor"
        );
    }

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn a_merge_that_would_collide_two_samples_is_refused_and_changes_nothing() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let source_stream = create_paired_stream(&db, "merge-collide-src", PARAM_S1_TEMP_ID).await;
    let target_stream = create_paired_stream(&db, "merge-collide-dst", PARAM_S1_DO_ID).await;
    let at = format!("{DAY}T09:00:00Z");
    add_sample(&db, GLOBAL_PARAM_TEMP_ID, &at, source_stream, 310.0).await;
    add_sample(&db, GLOBAL_PARAM_DO_ID, &at, target_stream, 410.0).await;

    let outcome = merge_site_parameters(&db, &request()).await;
    let Err(AppError::Conflict(message)) = outcome else {
        panic!("two groups collected at one instant cannot be combined: {outcome:?}");
    };
    assert!(
        message.contains("samples") && message.contains("09:00:00"),
        "the refusal names the colliding instant: {message}"
    );

    assert_eq!(
        rows_for(&db, "samples", GLOBAL_PARAM_TEMP_ID).await,
        1,
        "a refused merge moves nothing"
    );
    assert_eq!(rows_for(&db, "readings", GLOBAL_PARAM_TEMP_ID).await, 1);
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint FROM site_parameters WHERE id = '{PARAM_S1_TEMP_ID}'"
            )
        )
        .await,
        1,
        "and the source slot survives, so the operator can resolve the collision"
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn merging_an_empty_slot_reports_no_rows_and_still_deletes_it() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let result = merge_site_parameters(&db, &request())
        .await
        .expect("a slot with no data merges");
    assert_eq!(result.merged_readings, 0);
    assert_eq!(result.merged_status_events, 0);
    assert!(result.source_deleted);
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint FROM site_parameters WHERE id = '{PARAM_S1_TEMP_ID}'"
            )
        )
        .await,
        0
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn a_merge_onto_itself_is_refused() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let same = MergeSiteParametersRequest {
        source_site_parameter_id: PARAM_S1_TEMP_ID.parse().unwrap(),
        target_site_parameter_id: PARAM_S1_TEMP_ID.parse().unwrap(),
    };
    assert!(matches!(
        merge_site_parameters(&db, &same).await,
        Err(AppError::BadRequest(_))
    ));

    cleanup_test_db(&db).await;
}
