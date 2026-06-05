//! Subtle sensor-attribution rules from the lifecycle doc (§3 gap-absorbing calibration windows, §6
//! the recall-NULL guard) that the broader suite exercises only indirectly. Each drives the real
//! reprocess engine through the HTTP API and asserts per-reading outcomes.
//!
//! Run: cargo test --test sensor_edge_cases_test -- --test-threads=1

mod common;

use common::sensor_lifecycle::*;
use common::*;
use serial_test::serial;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(30);

/// §3 — calibration windows are gap-absorbing: a window runs to the NEXT calibration's `valid_from`,
/// so a reading between two calibrations resolves to the EARLIER one, never the later or the prior.
#[tokio::test]
#[serial]
async fn calibration_window_absorbs_gap_to_next_valid_from() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Probe-gap", GLOBAL_PARAM_TEMP_ID).await;
    let dep = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    let stream = create_paired_stream(&db, "gap-probe", PARAM_S1_TEMP_ID).await;
    insert_readings(
        &db, stream, SITE1_ID, GLOBAL_PARAM_TEMP_ID,
        sensor.id, sensor.identity_calibration_id, dep, 1.0, 0.0,
        &[
            (dt("2025-01-05T00:00:00Z"), 10.0), // before C1 → identity
            (dt("2025-01-15T00:00:00Z"), 20.0), // inside C1's window [01-10, 01-20)
            (dt("2025-01-25T00:00:00Z"), 30.0), // inside C2's window [01-20, ∞)
        ],
    ).await;

    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    for (slope, valid_from) in [(2.0, "2025-01-10T00:00:00Z"), (3.0, "2025-01-20T00:00:00Z")] {
        let (status, body) = post_json_with_token(
            &app, "/api/sensor_calibrations",
            &serde_json::json!({"sensor_id": sensor.id, "slope": slope, "intercept": 0.0, "valid_from": valid_from}),
            &token,
        ).await;
        assert_eq!(status, 201, "create calibration ({status}): {body}");
        assert!(wait_for_reprocessing(&db, sensor.id, WAIT).await, "reprocess after slope={slope}");
    }

    let rows = get_readings(&db, stream).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].calibrated_value, Some(10.0), "01-05 uses identity (1*10)");
    assert_eq!(rows[1].calibrated_value, Some(40.0), "01-15 uses C1 (2*20) — gap absorbed to C2's start");
    assert_eq!(rows[2].calibrated_value, Some(90.0), "01-25 uses C2 (3*30)");
}

/// §6 — the recall-NULL guard. Readings at or after the sensor's first `deployed_from` that fall in
/// a deployment gap are un-attributed (site_id NULL); readings that PREDATE the first deployment keep
/// the site_id pairing gave them and are never silently un-attributed.
#[tokio::test]
#[serial]
async fn recall_keeps_pre_first_deployment_readings_attributed() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Probe-recall", GLOBAL_PARAM_TEMP_ID).await;
    let dep = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-01-10T00:00:00Z")).await;
    let stream = create_paired_stream(&db, "recall-guard", PARAM_S1_TEMP_ID).await;
    insert_readings(
        &db, stream, SITE1_ID, GLOBAL_PARAM_TEMP_ID,
        sensor.id, sensor.identity_calibration_id, dep, 1.0, 0.0,
        &[
            (dt("2025-01-05T00:00:00Z"), 5.0),  // BEFORE first deployment (01-10)
            (dt("2025-01-11T00:00:00Z"), 11.0), // inside [01-10, 01-12)
            (dt("2025-01-15T00:00:00Z"), 15.0), // after recall → deployment gap
        ],
    ).await;

    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    // Recall: close the deployment at 01-12.
    let (status, body) = put_json_with_token(
        &app, &format!("/api/sensor_deployments/{dep}"),
        &serde_json::json!({"deployed_until": "2025-01-12T00:00:00Z"}),
        &token,
    ).await;
    assert_eq!(status, 200, "recall ({status}): {body}");
    assert!(wait_for_reprocessing(&db, sensor.id, WAIT).await, "reprocess after recall");

    let site1: uuid::Uuid = SITE1_ID.parse().unwrap();
    let rows = get_readings(&db, stream).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].site_id, Some(site1), "01-05 predates first deployment → keeps pairing's site_id");
    assert_eq!(rows[1].site_id, Some(site1), "01-11 inside the (now-closed) window → still SITE1");
    assert_eq!(rows[2].site_id, None, "01-15 in the post-recall gap → un-attributed");
}
