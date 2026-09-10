//! Import vs Adopt vs Swap (Phase 2): a stream's sensor is imported into inventory without a site,
//! then explicitly adopted to a site slot (which backfills its readings by window), the slot is
//! single-occupancy, and a swap ends one sensor and starts another at the same instant.
//!
//! Run: cargo test --test sensors -- --test-threads=1

use crate::common::sensor_lifecycle as sl;
use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;
use uuid::Uuid;

/// Create an UNPAIRED stream (no site_parameter, no sensor) with site-less readings.
async fn seed_unpaired_stream(db: &sea_orm::DatabaseConnection, source_key: &str) -> Uuid {
    let stream = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            // Explicitly instrument-less: the harness defaults `sensor_id` to the fixture
            // instrument, and this stream is the state an import exists to repair.
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active, sensor_id) \
             VALUES ('{stream}', 'test', '{source_key}', 'Imp {source_key}', true, NULL)"
        ),
    )
    .await;
    // The rows predate the instrument rule, which is the state an import exists to repair and the
    // only way to reach it: nothing writes an unattributed reading any more.
    let rows: Vec<String> = (0..6)
        .map(|i| {
            format!(
                "INSERT INTO readings (stream_id, time, raw_value, replicate_index) \
                 VALUES ('{stream}', '2025-06-01T00:{:02}:00Z', {}, 0)",
                i * 10,
                10.0 + f64::from(i)
            )
        })
        .collect();
    crate::common::db::seed_before_instrument_rule(db, &rows).await;
    stream
}

#[tokio::test]
#[serial]
async fn import_then_adopt_backfills_by_window() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let site1 = Uuid::parse_str(crate::common::SITE1_ID).unwrap();
    let temp = Uuid::parse_str(crate::common::GLOBAL_PARAM_TEMP_ID).unwrap();

    let stream = seed_unpaired_stream(&db, "import-adopt").await;

    // IMPORT: sensor created, readings get sensor_id but NO site/deployment.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream}/import"),
        &serde_json::json!({ "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "import ({status}): {body}");
    let imp: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        imp["attributed"].as_u64().unwrap(),
        6,
        "all six readings attributed"
    );
    let sensor_id = imp["sensor_id"].as_str().unwrap().to_string();

    // The rule holds again only if the import left nothing unattributed behind.
    crate::common::db::assert_instrument_rule_holds(&db).await;

    let rows = sl::get_readings(&db, stream).await;
    assert_eq!(rows.len(), 6);
    for r in &rows {
        assert!(r.sensor_id.is_some(), "import stamps sensor_id");
        assert_eq!(r.site_id, None, "import does NOT attribute to a site");
        assert_eq!(r.deployment_id, None, "import does NOT deploy");
    }
    // Re-import is idempotent.
    let (_s, body2) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream}/import"),
        &serde_json::json!({ "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID }),
        &token,
    )
    .await;
    let imp2: serde_json::Value = serde_json::from_str(&body2).unwrap();
    assert_eq!(
        imp2["attributed"].as_u64().unwrap(),
        0,
        "re-import attributes nothing new"
    );
    assert_eq!(
        imp2["sensor_id"].as_str().unwrap(),
        sensor_id,
        "same sensor reused"
    );

    // ADOPT: deploy from before the first reading → reprocess backfills site + deployment + parameter.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sensors/{sensor_id}/adopt"),
        &serde_json::json!({ "site_id": crate::common::SITE1_ID, "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID, "deployed_from": "2025-05-01T00:00:00Z" }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "adopt ({status}): {body}");
    let adopt: serde_json::Value = serde_json::from_str(&body).unwrap();
    let job_id = adopt["job_id"].as_str().unwrap();
    assert_eq!(
        crate::common::e2e::poll_job(&app, &token, job_id, 30).await,
        "completed",
        "adopt reprocess job completes"
    );

    let rows = sl::get_readings(&db, stream).await;
    for r in &rows {
        assert_eq!(r.site_id, Some(site1), "adopt backfills site_id");
        assert_eq!(r.parameter_id, Some(temp), "adopt backfills parameter_id");
        assert!(r.deployment_id.is_some(), "adopt backfills deployment_id");
    }
}

#[tokio::test]
#[serial]
async fn adopt_rejects_occupied_slot() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor_a = sl::create_sensor(&db, "occ-a", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let sensor_b = sl::create_sensor(&db, "occ-b", crate::common::GLOBAL_PARAM_TEMP_ID).await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sensors/{}/adopt", sensor_a.id),
        &serde_json::json!({ "site_id": crate::common::SITE1_ID, "deployed_from": "2025-06-01T00:00:00Z" }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "adopt A ({status}): {body}");

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sensors/{}/adopt", sensor_b.id),
        &serde_json::json!({ "site_id": crate::common::SITE1_ID, "deployed_from": "2025-06-02T00:00:00Z" }),
        &token,
    )
    .await;
    assert_eq!(
        status, 409,
        "adopting B into A's occupied slot must conflict: {body}"
    );
}

#[tokio::test]
#[serial]
async fn swap_ends_a_starts_b() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor_a = sl::create_sensor(&db, "swap-a", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let sensor_b = sl::create_sensor(&db, "swap-b", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    sl::deploy_sensor(
        &db,
        sensor_a.id,
        crate::common::SITE1_ID,
        sl::dt("2025-06-01T00:00:00Z"),
    )
    .await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/actions/swap",
        &serde_json::json!({
            "outgoing_sensor_id": sensor_a.id,
            "incoming_sensor_id": sensor_b.id,
            "site_id": crate::common::SITE1_ID,
            "at": "2025-06-15T00:00:00Z"
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "swap ({status}): {body}");
    let swap: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        swap["ended_deployment_id"].is_string(),
        "A's deployment was ended"
    );
    assert!(
        crate::common::e2e::poll_job(&app, &token, swap["incoming_job_id"].as_str().unwrap(), 30)
            .await
            == "completed"
    );

    // A's deployment closed at the swap instant; exactly one deployment covers any later instant.
    let open_count: i64 = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*) AS c FROM sensor_deployments \
             WHERE site_id = $1 AND parameter_id = $2 AND deployed_until IS NULL",
            [
                Uuid::parse_str(crate::common::SITE1_ID).unwrap().into(),
                Uuid::parse_str(crate::common::GLOBAL_PARAM_TEMP_ID)
                    .unwrap()
                    .into(),
            ],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "c")
        .unwrap();
    assert_eq!(open_count, 1, "exactly one open deployment (B) after swap");
}
