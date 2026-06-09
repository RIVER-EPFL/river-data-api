//! Bulk historical attribution: readings ingested before a sensor's deployment existed sit with
//! `sensor_id NULL` (the import-backfill orphan state). `GET /actions/backfill_candidates` surfaces the
//! open deployments that have such claimable history, and `POST /actions/backfill_attribution`
//! backdates each deployment to its earliest claimable reading and window-reprocesses the slot so the
//! orphans get attributed. A prior deployment bounds how far back the open one can move.
//!
//! Run: cargo test --test e2e -- --test-threads=1


use crate::common::e2e;
use crate::common::sensor_lifecycle as sl;
use sea_orm::{ConnectionTrait, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

async fn count_slot_orphans(db: &sea_orm::DatabaseConnection) -> i64 {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COUNT(*) AS n FROM readings WHERE site_id = $1::uuid AND parameter_id = $2::uuid AND sensor_id IS NULL",
            [crate::common::SITE1_ID.into(), crate::common::GLOBAL_PARAM_TEMP_ID.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    row.try_get("", "n").unwrap()
}

async fn deployed_from(db: &sea_orm::DatabaseConnection, dep: Uuid) -> String {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT deployed_from FROM sensor_deployments WHERE id = $1",
            [dep.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    let t: chrono::DateTime<chrono::FixedOffset> = row.try_get("", "deployed_from").unwrap();
    t.to_rfc3339()
}

#[tokio::test]
#[serial]
async fn backfill_attributes_pre_deployment_history() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    // Sensor deployed from T0; one in-window reading + three pre-T0 orphans at the same slot.
    let sensor = sl::create_sensor(&db, "backfill-probe", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let t0 = sl::dt("2025-06-01T00:00:00Z");
    let dep = sl::deploy_sensor(&db, sensor.id, crate::common::SITE1_ID, t0).await;
    let stream = sl::create_paired_stream(&db, "backfill", crate::common::PARAM_S1_TEMP_ID).await;
    sl::insert_readings(
        &db, stream, crate::common::SITE1_ID, crate::common::GLOBAL_PARAM_TEMP_ID,
        sensor.id, sensor.identity_calibration_id, dep, 1.0, 0.0,
        &[(sl::dt("2025-06-02T00:00:00Z"), 20.0)],
    )
    .await;
    sl::insert_orphan_readings(
        &db, stream, crate::common::SITE1_ID, crate::common::GLOBAL_PARAM_TEMP_ID,
        &[
            (sl::dt("2025-03-01T00:00:00Z"), 10.0),
            (sl::dt("2025-04-01T00:00:00Z"), 11.0),
            (sl::dt("2025-05-01T00:00:00Z"), 12.0),
        ],
    )
    .await;

    // Precondition: the gap exists.
    assert_eq!(count_slot_orphans(&db).await, 3, "three pre-deployment orphans seeded");

    // Candidates surface this deployment with the right target + count.
    let (status, body) =
        crate::common::get_json_with_token(&app, "/api/actions/backfill_candidates", &token).await;
    assert_eq!(status, 200, "candidates: {body}");
    let cands = body["candidates"].as_array().unwrap();
    assert_eq!(cands.len(), 1, "one candidate: {body}");
    assert_eq!(cands[0]["deployment_id"].as_str().unwrap(), dep.to_string());
    assert_eq!(cands[0]["claimable_count"].as_i64().unwrap(), 3);
    assert_eq!(&cands[0]["target_from"].as_str().unwrap()[..10], "2025-03-01");
    assert_eq!(body["total_claimable"].as_i64().unwrap(), 3);

    // Backfill all → backdate + window-reprocess.
    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/backfill_attribution",
        &json!({ "all": true }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "backfill: {resp}");
    assert_eq!(resp["deployments_updated"].as_i64().unwrap(), 1);
    let job_id = resp["job_id"].as_str().unwrap();
    assert_eq!(e2e::poll_job(&app, &token, job_id, 30).await, "completed", "job completes");

    // Every reading at the slot is now attributed to the sensor, and deployed_from moved back.
    let rows = sl::get_readings(&db, stream).await;
    assert_eq!(rows.len(), 4);
    for r in &rows {
        assert_eq!(r.sensor_id, Some(sensor.id), "reading {} attributed", r.time);
        assert_eq!(r.deployment_id, Some(dep), "reading {} deployment", r.time);
        assert!(r.calibration_id.is_some(), "reading {} calibration", r.time);
    }
    assert_eq!(count_slot_orphans(&db).await, 0, "no orphans remain");
    assert_eq!(&deployed_from(&db, dep).await[..10], "2025-03-01", "deployed_from backdated");

    // Candidates now empty.
    let (_s, body2) =
        crate::common::get_json_with_token(&app, "/api/actions/backfill_candidates", &token).await;
    assert_eq!(body2["total_candidates"].as_i64().unwrap(), 0, "no candidates left");
}

#[tokio::test]
#[serial]
async fn backfill_is_bounded_by_a_prior_deployment() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    // Prior sensor A held the slot [2025-01-01, 2025-04-15); sensor B is the current open deployment.
    let a = sl::create_sensor(&db, "prior-A", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let dep_a = sl::deploy_sensor(&db, a.id, crate::common::SITE1_ID, sl::dt("2025-01-01T00:00:00Z")).await;
    sl::end_deployment(&db, dep_a, sl::dt("2025-04-15T00:00:00Z")).await;
    let b = sl::create_sensor(&db, "current-B", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let dep_b = sl::deploy_sensor(&db, b.id, crate::common::SITE1_ID, sl::dt("2025-06-01T00:00:00Z")).await;

    // Orphan inside A's window + orphan in the gap between A and B.
    let stream = sl::create_paired_stream(&db, "bounded", crate::common::PARAM_S1_TEMP_ID).await;
    sl::insert_orphan_readings(
        &db, stream, crate::common::SITE1_ID, crate::common::GLOBAL_PARAM_TEMP_ID,
        &[
            (sl::dt("2025-03-01T00:00:00Z"), 10.0), // in A's window
            (sl::dt("2025-05-01T00:00:00Z"), 12.0), // gap (claimable by B)
        ],
    )
    .await;

    // B's candidate only counts the gap orphan; target is bounded at A's end (2025-04-15).
    let (_s, body) =
        crate::common::get_json_with_token(&app, "/api/actions/backfill_candidates", &token).await;
    let cand = body["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["deployment_id"] == dep_b.to_string())
        .expect("B is a candidate");
    assert_eq!(cand["claimable_count"].as_i64().unwrap(), 1, "only the gap orphan: {body}");
    assert_eq!(&cand["target_from"].as_str().unwrap()[..10], "2025-05-01");

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/backfill_attribution",
        &json!({ "deployment_ids": [dep_b.to_string()] }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "backfill (no overlap error): {resp}");
    assert_eq!(e2e::poll_job(&app, &token, resp["job_id"].as_str().unwrap(), 30).await, "completed");

    // 2025-05-01 → B; 2025-03-01 → A (its window covers it); deployed_from(B) bounded at the gap start.
    let rows = sl::get_readings(&db, stream).await;
    let by_time = |d: &str| {
        rows.iter()
            .find(|r| r.time.to_rfc3339().starts_with(d))
            .unwrap()
            .sensor_id
    };
    assert_eq!(by_time("2025-05-01"), Some(b.id), "gap reading attributed to B");
    assert_eq!(by_time("2025-03-01"), Some(a.id), "in-A-window reading attributed to A");
    assert_eq!(&deployed_from(&db, dep_b).await[..10], "2025-05-01", "B bounded at gap, not into A");
}
