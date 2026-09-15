//! Scenario: a (site, parameter) slot stops owning its data, either because one stream stops
//! feeding it (unpair) or because the slot itself is going away (site_parameter delete).
//!
//! Expected behaviour: `retire_slot` releases everything its scope names in one transaction, counts
//! only the rows it actually released, deletes the samples nothing references any more, unpairs the
//! streams pointing at a slot that is dying, and is safe to run again.
//!
//! Run: cargo test --test data_streams slot_retirement -- --test-threads=1

use river_db::routes::private::data_streams::flows::retire_slot;
use river_db::routes::private::data_streams::models::SlotScope;
use serial_test::serial;
use uuid::Uuid;

use crate::common::sensor_lifecycle::{
    create_paired_stream, create_unpaired_stream, seed_base_entities,
};
use crate::common::*;

const HOUR: &str = "2025-10-06T09";

async fn attributed_reading(db: &sea_orm::DatabaseConnection, stream: Uuid, minute: u32, v: f64) {
    exec(
        db,
        &format!(
            "INSERT INTO readings \
             (stream_id, site_id, parameter_id, time, raw_value, calibrated_value, replicate_index) \
             VALUES ('{stream}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', \
                     '{HOUR}:{minute:02}:00Z', {v}, {v}, 0)"
        ),
    )
    .await;
}

async fn unattributed_reading(db: &sea_orm::DatabaseConnection, stream: Uuid, minute: u32, v: f64) {
    exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, time, raw_value, replicate_index) \
             VALUES ('{stream}', '{HOUR}:{minute:02}:00Z', {v}, 0)"
        ),
    )
    .await;
}

async fn attributed_count(db: &sea_orm::DatabaseConnection, stream: Uuid) -> i64 {
    e2e::count(
        db,
        &format!(
            "SELECT COUNT(*)::bigint FROM readings \
             WHERE stream_id = '{stream}' AND site_id IS NOT NULL"
        ),
    )
    .await
}

#[tokio::test]
#[serial]
async fn retiring_a_stream_counts_only_the_rows_it_releases() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let stream = create_paired_stream(&db, "retire-partial", PARAM_S1_TEMP_ID).await;
    for (i, v) in [10.0, 20.0, 30.0].iter().enumerate() {
        attributed_reading(&db, stream, i as u32, *v).await;
    }
    // Rows the stream wrote before it was paired: already released, so re-releasing them would
    // both inflate the count and decompress rows the operation changes nothing in.
    unattributed_reading(&db, stream, 10, 40.0).await;
    unattributed_reading(&db, stream, 11, 50.0).await;

    let touched = retire_slot(&db, SlotScope::Stream(stream))
        .await
        .expect("retiring a stream's rows");

    assert_eq!(
        touched.touched.rows, 3,
        "only the attributed rows are released"
    );
    assert_eq!(attributed_count(&db, stream).await, 0);
    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT COUNT(*)::bigint FROM readings WHERE stream_id = '{stream}'")
        )
        .await,
        5,
        "releasing a slot hides readings from the rollups, it does not delete them"
    );
    let (min_time, max_time) = touched
        .touched
        .span()
        .expect("a non-empty release reports its span");
    assert_eq!(min_time.to_rfc3339(), format!("{HOUR}:00:00+00:00"));
    assert_eq!(max_time.to_rfc3339(), format!("{HOUR}:02:00+00:00"));

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn retiring_a_stream_a_second_time_releases_nothing() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let stream = create_paired_stream(&db, "retire-twice", PARAM_S1_TEMP_ID).await;
    attributed_reading(&db, stream, 0, 10.0).await;

    assert_eq!(
        retire_slot(&db, SlotScope::Stream(stream))
            .await
            .expect("first release")
            .touched
            .rows,
        1
    );
    let again = retire_slot(&db, SlotScope::Stream(stream))
        .await
        .expect("a second release is not an error");
    assert!(again.touched.is_empty(), "nothing is left to release");
    assert!(
        again.touched.span().is_none(),
        "and so no rollup window is implied"
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn retiring_a_stream_recomputes_the_visits_it_empties() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let output_parameter = Uuid::new_v4();
    let output_slot = Uuid::new_v4();
    let calculation = Uuid::new_v4();
    let version = Uuid::new_v4();
    for statement in [
        format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
             VALUES ('{output_parameter}', 'RetireOutput', 'Retire output', 'x', 'measurement')"
        ),
        format!(
            "INSERT INTO site_parameters \
                 (id, site_id, parameter_id, name, sensor_type, entry_mode, is_active) \
             VALUES ('{output_slot}', '{SITE1_ID}', '{output_parameter}', \
                     'Retire output', 'derived', 'tool', true)"
        ),
        format!(
            "INSERT INTO tool_scripts (id, name, label, created_by) \
             VALUES ('{calculation}', 'retire_output', 'Retire output', 'test')"
        ),
        format!(
            r#"INSERT INTO tool_script_versions
                   (id, tool_script_id, version_no, script, entry_function, manifest, test_cases,
                    content_hash, created_by, validated_at)
               VALUES ('{version}', '{calculation}', 1, 'tool <- function(...) list()', 'tool',
                       '{{"label":"Retire output","params":[{{"name":"t","label":"Temperature","kind":"number","required":true}}],"event_inputs":[{{"param":"t","parameter_code":"DO_Temperature"}}],"outputs":[]}}'::jsonb,
                       '{{}}'::jsonb, 'retire-output-v1', 'test', now())"#
        ),
        format!(
            "UPDATE tool_scripts SET active_version_id = '{version}' WHERE id = '{calculation}'"
        ),
    ] {
        exec(&db, &statement).await;
    }

    let source = create_paired_stream(&db, "retire-visit-input", PARAM_S1_TEMP_ID).await;
    let output = create_paired_stream(&db, "retire-visit-output", &output_slot.to_string()).await;
    let event = Uuid::new_v4();
    for statement in [
        format!(
            "INSERT INTO collection_events (id, site_id, collected_at, source) \
             VALUES ('{event}', '{SITE1_ID}', '{HOUR}:00:00Z', 'manual')"
        ),
        format!(
            "INSERT INTO readings \
                 (stream_id, site_id, parameter_id, time, raw_value, replicate_index, \
                  measurement_type, collection_event_id) \
             VALUES \
                 ('{source}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{HOUR}:00:00Z', 10, 0, \
                  'spot', '{event}'), \
                 ('{output}', '{SITE1_ID}', '{output_parameter}', '{HOUR}:00:00Z', 20, 0, \
                  'spot', '{event}')"
        ),
    ] {
        exec(&db, &statement).await;
    }

    let token = seed_api_token(&db, full_permissions(), None).await;
    let app = build_test_app(db.clone());
    let (status, body) = post_json_with_token(
        &app,
        &format!("/api/streams/{source}/unpair"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "unpair ({status}): {body}");

    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint FROM reprocessing_jobs \
                 WHERE trigger_type = 'event_recompute' AND trigger_id = '{event}'"
            )
        )
        .await,
        1,
        "the stored output is recomputed after one of its inputs leaves the visit"
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn retiring_a_slot_releases_every_stream_and_deletes_its_orphaned_samples() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let logger = create_paired_stream(&db, "retire-slot-logger", PARAM_S1_TEMP_ID).await;
    let grab = create_paired_stream(&db, "retire-slot-grab", PARAM_S1_TEMP_ID).await;
    attributed_reading(&db, logger, 0, 10.0).await;

    let sample_id = Uuid::new_v4();
    exec(
        &db,
        &format!(
            "INSERT INTO samples (id, site_id, parameter_id, collected_at) \
             VALUES ('{sample_id}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{HOUR}:30:00Z')"
        ),
    )
    .await;
    for replicate in 0..2 {
        exec(
            &db,
            &format!(
                "INSERT INTO readings \
                 (stream_id, site_id, parameter_id, time, raw_value, calibrated_value, \
                  replicate_index, sample_id, measurement_type) \
                 VALUES ('{grab}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{HOUR}:30:00Z', \
                         {}.0, {}.0, {replicate}, '{sample_id}', 'spot')",
                20 + replicate,
                20 + replicate
            ),
        )
        .await;
    }

    let touched = retire_slot(
        &db,
        SlotScope::SiteParameter(PARAM_S1_TEMP_ID.parse().unwrap()),
    )
    .await
    .expect("retiring the slot");

    assert_eq!(
        touched.touched.rows, 3,
        "the slot owns every row attributed to it, whichever stream wrote it"
    );
    assert_eq!(attributed_count(&db, logger).await, 0);
    assert_eq!(attributed_count(&db, grab).await, 0);
    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT COUNT(*)::bigint FROM samples WHERE id = '{sample_id}'")
        )
        .await,
        0,
        "a sample no reading references any more describes nothing"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint FROM data_streams \
                 WHERE site_parameter_id = '{PARAM_S1_TEMP_ID}'"
            )
        )
        .await,
        0,
        "a slot that is going away releases the streams pointing at it, or the row cannot be \
         deleted at all"
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn retiring_a_slot_leaves_its_neighbours_alone() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let target = create_paired_stream(&db, "retire-target", PARAM_S1_TEMP_ID).await;
    let bystander = create_paired_stream(&db, "retire-bystander", PARAM_S1_DO_ID).await;
    attributed_reading(&db, target, 0, 10.0).await;
    exec(
        &db,
        &format!(
            "INSERT INTO readings \
             (stream_id, site_id, parameter_id, time, raw_value, calibrated_value, replicate_index) \
             VALUES ('{bystander}', '{SITE1_ID}', '{GLOBAL_PARAM_DO_ID}', \
                     '{HOUR}:00:00Z', 99.0, 99.0, 0)"
        ),
    )
    .await;

    retire_slot(
        &db,
        SlotScope::SiteParameter(PARAM_S1_TEMP_ID.parse().unwrap()),
    )
    .await
    .expect("retiring the slot");

    assert_eq!(attributed_count(&db, target).await, 0);
    assert_eq!(
        attributed_count(&db, bystander).await,
        1,
        "another slot at the same site keeps its attribution"
    );

    cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn retiring_a_slot_that_is_already_gone_is_not_an_error() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let touched = retire_slot(&db, SlotScope::SiteParameter(Uuid::new_v4()))
        .await
        .expect("an unresolvable slot reports an empty range rather than failing");
    assert!(touched.touched.is_empty());

    let never_used = create_unpaired_stream(&db, "retire-empty").await;
    assert!(
        retire_slot(&db, SlotScope::Stream(never_used))
            .await
            .expect("a stream with no rows is not an error")
            .touched
            .is_empty()
    );

    cleanup_test_db(&db).await;
}
