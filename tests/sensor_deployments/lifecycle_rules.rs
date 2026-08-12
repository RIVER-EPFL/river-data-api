//! Subtle sensor-attribution rules from the lifecycle doc (§3 gap-absorbing calibration windows, §6
//! the recall-NULL guard) that the broader suite exercises only indirectly. Each drives the real
//! reprocess engine through the HTTP API and asserts per-reading outcomes.
//!
//! Run: cargo test --test sensor_deployments -- --test-threads=1


use crate::common::e2e;
use crate::common::sensor_lifecycle::*;
use crate::common::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(30);

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    let row = db
        .query_one(Statement::from_string(DatabaseBackend::Postgres, sql.to_string()))
        .await
        .expect("query")
        .expect("row");
    row.try_get::<i64>("", "c").expect("c")
}

async fn deployed_until(db: &DatabaseConnection, id: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT deployed_until FROM sensor_deployments WHERE id = '{id}'"),
        ))
        .await
        .ok()
        .flatten()?;
    row.try_get::<chrono::DateTime<chrono::FixedOffset>>("", "deployed_until")
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

/// H8/H9, a multi-channel instrument holds one open deployment per parameter. Deploying a second
/// channel (a different parameter) at the same site must NOT auto-recall the first channel: the recall
/// is scoped to the parameter being deployed.
#[tokio::test]
#[serial]
async fn deploying_a_second_channel_keeps_the_first_open() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "MultiChan-01", GLOBAL_PARAM_TEMP_ID).await;
    let temp_dep = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    assert!(deployed_until(&db, &temp_dep.to_string()).await.is_none(), "temperature channel starts open");

    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;
    // Deploy the SAME sensor for a DIFFERENT parameter (DO) at the same site via the create hook.
    let (status, body) = post_json_with_token(
        &app,
        "/api/sensor_deployments",
        &serde_json::json!({
            "sensor_id": sensor.id,
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_DO_ID,
            "deployed_from": "2025-06-01T00:00:00Z",
            "deployment_type": "permanent",
        }),
        &token,
    )
    .await;
    assert!(status == 200 || status == 201, "deploy DO channel: {status} {body}");

    assert!(
        deployed_until(&db, &temp_dep.to_string()).await.is_none(),
        "the temperature channel stays open when the DO channel is deployed, recall is parameter-scoped"
    );
    let open = count(
        &db,
        &format!(
            "SELECT COUNT(*) AS c FROM sensor_deployments \
             WHERE sensor_id = '{}' AND deployed_until IS NULL",
            sensor.id
        ),
    )
    .await;
    assert_eq!(open, 2, "both channels are open concurrently");

    cleanup_test_db(&db).await;
}

/// §3, calibration windows are gap-absorbing: a window runs to the NEXT calibration's `valid_from`,
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
    assert_eq!(rows[1].calibrated_value, Some(40.0), "01-15 uses C1 (2*20), gap absorbed to C2's start");
    assert_eq!(rows[2].calibrated_value, Some(90.0), "01-25 uses C2 (3*30)");
}

/// §6, the recall-NULL guard. Readings at or after the sensor's first `deployed_from` that fall in
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

/// §3, deployment windows are gap-preserving: `recompute_deployed_until` only ever SHORTENS
/// (`LEAST(existing, LEAD(deployed_from))`), never extends. Trying to push a window past the next
/// deployment's start is re-clamped, so deployments never overlap.
#[tokio::test]
#[serial]
async fn deployment_recompute_only_shortens_never_extends() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Probe-shorten", GLOBAL_PARAM_TEMP_ID).await;
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    // Deploy A at SITE1, then B at SITE2 two hours later (auto-recall closes A at B's start).
    let sid = sensor.id.to_string();
    let dep_a = e2e::create_deployment(&app, &token, &sid, SITE1_ID, GLOBAL_PARAM_TEMP_ID, "2025-03-01T00:00:00Z").await;
    let _dep_b = e2e::create_deployment(&app, &token, &sid, SITE2_ID, GLOBAL_PARAM_TEMP_ID, "2025-03-01T02:00:00Z").await;
    assert!(wait_for_reprocessing(&db, sensor.id, WAIT).await, "reprocess after second deploy");
    assert_eq!(
        deployed_until(&db, &dep_a).await,
        Some(dt("2025-03-01T02:00:00Z")),
        "A auto-closed at B's start"
    );

    // Attempt to EXTEND A past B's start. Different sites, so no slot conflict, but recompute must
    // re-clamp A back to B's start rather than letting it overlap.
    let (status, body) = put_json_with_token(
        &app,
        &format!("/api/sensor_deployments/{dep_a}"),
        &serde_json::json!({"deployed_until": "2025-03-01T05:00:00Z"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "extend attempt ({status}): {body}");
    assert!(wait_for_reprocessing(&db, sensor.id, WAIT).await, "reprocess after extend attempt");
    assert_eq!(
        deployed_until(&db, &dep_a).await,
        Some(dt("2025-03-01T02:00:00Z")),
        "recompute re-clamped A to B's start, never extended"
    );
}

/// §4/§5, pairing a stream whose `(site, parameter)` slot is already held by another sensor must
/// not raise: auto-deploy is skipped (`find_or_create_deployment` returns None) and the readings
/// carry `sensor_id`/`calibration_id` but no `deployment_id` until an explicit adopt.
#[tokio::test]
#[serial]
async fn auto_deploy_skipped_when_slot_occupied() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    // Sensor A holds the (SITE1, TEMP) slot with an open deployment.
    let sensor_a = create_sensor(&db, "Incumbent-A", GLOBAL_PARAM_TEMP_ID).await;
    deploy_sensor(&db, sensor_a.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;

    // A second, unpaired stream (NULL serial → a distinct sensor B is created on pair) with readings.
    let stream = create_unpaired_stream(&db, "occupied-slot").await;
    insert_unpaired_readings(
        &db, stream,
        &[(dt("2025-02-01T00:00:00Z"), 10.0), (dt("2025-02-01T00:10:00Z"), 11.0)],
    )
    .await;

    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    // Pairing to the occupied slot must succeed (no 5xx), but skip auto-deploy for sensor B.
    let (status, body) = post_json_with_token(
        &app,
        &format!("/api/streams/{stream}/pair"),
        &serde_json::json!({"site_parameter_id": PARAM_S1_TEMP_ID}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "pair into occupied slot ({status}): {body}");

    // The pair handler spawns a pairing_backfill reprocess job that re-derives attribution by
    // deployment windows. Sensor A's open deployment covers the reading timestamps, so the
    // reprocess re-owns them to A.
    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "pairing_backfill", 30).await,
        "pairing_backfill job completes"
    );

    // After reprocess: readings are attributed to sensor A (the incumbent whose deployment covers
    // the timestamps), not sensor B (which was created for the stream but never deployed).
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings \
                 WHERE stream_id = '{stream}' AND sensor_id = '{}' AND deployment_id IS NOT NULL",
                sensor_a.id
            ),
        )
        .await,
        2,
        "readings re-attributed to the incumbent sensor A with its deployment"
    );

    // Sensor B was created for the stream (linked via data_streams.sensor_id) but has no
    // deployment, the slot was occupied. It stays available for an explicit adopt later.
    let stream_sensor = count(
        &db,
        &format!(
            "SELECT count(*) AS c FROM data_streams \
             WHERE id = '{stream}' AND sensor_id IS NOT NULL AND sensor_id <> '{}'",
            sensor_a.id
        ),
    )
    .await;
    assert_eq!(stream_sensor, 1, "stream is linked to a distinct sensor B (not A)");
}

/// §6, reprocess re-derives attribution from the timelines. Starting from readings whose
/// `calibration_id`/`deployment_id`/`calibrated_value` have been wiped, a manual reprocess restores
/// all of them by time window.
#[tokio::test]
#[serial]
async fn reprocess_restores_nulled_attribution_fks() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Probe-nulled", GLOBAL_PARAM_TEMP_ID).await;
    let dep = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    let stream = create_paired_stream(&db, "nulled-fks", PARAM_S1_TEMP_ID).await;
    insert_readings(
        &db, stream, SITE1_ID, GLOBAL_PARAM_TEMP_ID,
        sensor.id, sensor.identity_calibration_id, dep, 1.0, 0.0,
        &[(dt("2025-01-05T00:00:00Z"), 10.0)],
    )
    .await;

    // Corrupt the materialized attribution: wipe the FKs and the calibrated value.
    exec(
        &db,
        &format!(
            "UPDATE readings SET calibration_id = NULL, deployment_id = NULL, calibrated_value = 999 \
             WHERE stream_id = '{stream}'"
        ),
    )
    .await;

    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;
    let (status, body) = post_json_with_token(
        &app,
        "/api/actions/reprocess",
        &serde_json::json!({"sensor_id": sensor.id}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "reprocess ({status}): {body}");
    assert!(wait_for_reprocessing(&db, sensor.id, WAIT).await, "reprocess completes");

    let rows = get_readings(&db, stream).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].calibration_id, Some(sensor.identity_calibration_id), "calibration_id restored");
    assert_eq!(rows[0].deployment_id, Some(dep), "deployment_id restored");
    assert_eq!(rows[0].calibrated_value, Some(10.0), "calibrated_value re-derived (1*10), not 999");
    assert_eq!(rows[0].site_id, Some(SITE1_ID.parse::<uuid::Uuid>().unwrap()), "site_id intact");
}
