//! Swap handover (deferred Phase-1 part 2): when sensor B replaces sensor A at a (site, parameter)
//! slot, the feed's readings logged after the swap instant re-attribute from A to B by the
//! per-(site,parameter) reprocess, and the stream is relinked to B for future ingest. A per-sensor
//! reprocess cannot do this (post-swap rows still carry sensor A); the slot reprocess re-owns them.
//!
//! Run: cargo test --test e2e_sensor_swap_test -- --test-threads=1

mod common;

use common::e2e;
use common::sensor_lifecycle as sl;
use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;
use uuid::Uuid;

#[tokio::test]
#[serial]
async fn swap_reattributes_post_swap_readings() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());
    let site1 = Uuid::parse_str(common::SITE1_ID).unwrap();

    let sensor_a = sl::create_sensor(&db, "feed-a", common::GLOBAL_PARAM_TEMP_ID).await;
    let cal_a = sl::add_calibration(&db, sensor_a.id, 1.0, 0.0, sl::dt("2025-06-01T00:00:00Z")).await;
    let dep_a = sl::deploy_sensor(&db, sensor_a.id, common::SITE1_ID, sl::dt("2025-06-01T00:00:00Z")).await;
    let sensor_b = sl::create_sensor(&db, "feed-b", common::GLOBAL_PARAM_TEMP_ID).await;

    // One feed (paired stream) initially owned by A, with six readings.
    let stream = sl::create_paired_stream(&db, "feed", common::PARAM_S1_TEMP_ID).await;
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE data_streams SET sensor_id = $1 WHERE id = $2",
        [sensor_a.id.into(), stream.into()],
    ))
    .await
    .unwrap();
    let raw: Vec<(_, f64)> = (0..6)
        .map(|i| (sl::dt(&format!("2025-06-01T00:{:02}:00Z", i * 10)), 10.0 + i as f64))
        .collect();
    sl::insert_readings(
        &db, stream, common::SITE1_ID, common::GLOBAL_PARAM_TEMP_ID, sensor_a.id, cal_a, dep_a, 1.0, 0.0, &raw,
    )
    .await;

    // SWAP A → B at 00:30.
    let (status, body) = common::post_json_with_token(
        &app,
        "/api/actions/swap",
        &serde_json::json!({
            "outgoing_sensor_id": sensor_a.id,
            "incoming_sensor_id": sensor_b.id,
            "site_id": common::SITE1_ID,
            "at": "2025-06-01T00:30:00Z"
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "swap ({status}): {body}");
    assert!(
        e2e::wait_for_jobs_by_trigger(&db, "sensor_swap", 30).await,
        "the (site,parameter) handover reprocess job completes"
    );

    // Post-swap readings (>= 00:30) re-attributed to B; earlier ones stay A.
    let rows = sl::get_readings(&db, stream).await;
    assert_eq!(rows.len(), 6);
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(r.site_id, Some(site1), "reading[{i}] stays at site 1");
        if i < 3 {
            assert_eq!(r.sensor_id, Some(sensor_a.id), "reading[{i}] before swap stays sensor A");
        } else {
            assert_eq!(r.sensor_id, Some(sensor_b.id), "reading[{i}] after swap re-attributes to sensor B");
        }
    }

    // The feed is relinked to B for future ingest.
    let stream_sensor: Uuid = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sensor_id FROM data_streams WHERE id = $1",
            [stream.into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "sensor_id")
        .unwrap();
    assert_eq!(stream_sensor, sensor_b.id, "stream relinked to B");
}

#[tokio::test]
#[serial]
async fn swap_recalls_incoming_sensor_open_elsewhere() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());

    let sensor_a = sl::create_sensor(&db, "out", common::GLOBAL_PARAM_TEMP_ID).await;
    let sensor_b = sl::create_sensor(&db, "in", common::GLOBAL_PARAM_TEMP_ID).await;

    // A holds site 1; B is already deployed open at site 2 (a different slot).
    sl::deploy_sensor(&db, sensor_a.id, common::SITE1_ID, sl::dt("2025-06-01T00:00:00Z")).await;
    sl::deploy_sensor(&db, sensor_b.id, common::SITE2_ID, sl::dt("2025-06-01T00:00:00Z")).await;

    // Swap A→B at site 1. B's pre-existing open deployment at site 2 must be recalled so B isn't
    // left open at two sites at once.
    let (status, body) = common::post_json_with_token(
        &app,
        "/api/actions/swap",
        &serde_json::json!({
            "outgoing_sensor_id": sensor_a.id,
            "incoming_sensor_id": sensor_b.id,
            "site_id": common::SITE1_ID,
            "at": "2025-06-01T06:00:00Z"
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "swap ({status}): {body}");

    let open_b: i64 = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*) AS c FROM sensor_deployments \
             WHERE sensor_id = $1 AND deployed_until IS NULL",
            [sensor_b.id.into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "c")
        .unwrap();
    assert_eq!(open_b, 1, "incoming sensor must have exactly one open deployment after the swap");
}
