//! A PATCH that names another instrument on a deployment is held to the create's rule, and a PATCH
//! that moves a deployment to another instrument or site re-derives the slot and instrument it left
//! as well as the ones it now names.
//!
//! Run: cargo test --test sensor_deployments patch_moves_instrument -- --test-threads=1

use crate::common::e2e;
use crate::common::sensor_lifecycle as sl;
use serial_test::serial;

const RETIRED_INSTRUMENT: &str = "00000000-0000-4000-c000-0000000000d1";
const ENTRY_CHANNEL: &str = "00000000-0000-4000-c000-0000000000d2";

#[tokio::test]
#[serial]
async fn a_patch_cannot_name_an_instrument_a_create_refuses() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, is_active, kind, created_at) VALUES \
             ('{RETIRED_INSTRUMENT}', 'miniDOT 7392', false, 'device', now())"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, is_active, is_lab_instrument, kind, source_system, source_key, created_at) \
             VALUES ('{ENTRY_CHANNEL}', 'Upstream Temperature (grab entry)', true, true, \
                     'entry_channel', 'grab_sample', 'upstream:temp', now())"
        ),
    )
    .await;

    let sensor = sl::create_sensor(&db, "probe", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let dep = e2e::create_deployment(
        &app,
        &token,
        &sensor.id.to_string(),
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "2025-06-01T00:00:00Z",
    )
    .await;

    for (refused, why) in [
        (RETIRED_INSTRUMENT, "retired"),
        (ENTRY_CHANNEL, "entry_channel"),
    ] {
        let (status, body) = crate::common::put_json_with_token(
            &app,
            &format!("/api/sensor_deployments/{dep}"),
            &serde_json::json!({ "sensor_id": refused }),
            &token,
        )
        .await;
        assert_eq!(status, 400, "a PATCH naming {why} is refused: {body}");
        assert!(body.contains(why), "the refusal says why: {body}");
    }

    let owner = e2e::scalar(
        &db,
        &format!("SELECT sensor_id::text FROM sensor_deployments WHERE id = '{dep}'"),
    )
    .await;
    assert_eq!(
        owner,
        sensor.id.to_string(),
        "a refused PATCH changes nothing"
    );

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/sensor_deployments/{dep}"),
        &serde_json::json!({ "sensor_id": sensor.id, "notes": "same probe" }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "naming the same instrument again is not a move ({status}): {body}"
    );
}

#[tokio::test]
#[serial]
async fn moving_a_deployment_reprocesses_the_slot_and_instrument_it_left() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let first = sl::create_sensor(&db, "first", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let second = sl::create_sensor(&db, "second", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let dep = sl::deploy_sensor(
        &db,
        first.id,
        crate::common::SITE1_ID,
        sl::dt("2025-06-01T00:00:00Z"),
    )
    .await;
    let stream =
        sl::create_paired_stream(&db, "upstream-temp", crate::common::PARAM_S1_TEMP_ID).await;
    sl::insert_readings(
        &db,
        stream,
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        first.id,
        first.base_calibration_id,
        dep,
        1.0,
        0.0,
        &[
            (sl::dt("2025-06-02T00:00:00Z"), 10.0),
            (sl::dt("2025-06-03T00:00:00Z"), 11.0),
        ],
    )
    .await;

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/sensor_deployments/{dep}"),
        &serde_json::json!({ "sensor_id": second.id, "site_id": crate::common::SITE2_ID }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "move the deployment ({status}): {body}"
    );
    e2e::drain_jobs(&db, 60).await;

    let left = e2e::count(
        &db,
        &format!(
            "SELECT count(*) FROM reprocessing_jobs WHERE trigger_type = 'deployment_update' \
             AND params->>'sensor_id' = '{}' AND params->>'site_id' = '{}' \
             AND params->>'parameter_id' = '{}' AND status = 'completed'",
            first.id,
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    assert_eq!(
        left,
        1,
        "the slot and instrument the move left are reprocessed: {}",
        e2e::jobs_summary(&db).await
    );

    let stale = e2e::count(
        &db,
        &format!(
            "SELECT count(*) FROM readings WHERE stream_id = '{stream}' AND deployment_id = '{dep}'"
        ),
    )
    .await;
    assert_eq!(
        stale, 0,
        "the first probe's readings at the first site no longer name a deployment that now says \
         the second probe at the second site"
    );
}
