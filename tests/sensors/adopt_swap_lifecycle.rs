//! Adopt vs Swap: an instrument that owns a stream's readings but no site is explicitly
//! adopted to a site slot (which backfills its readings by window), the slot is single-occupancy,
//! and a swap ends one sensor and starts another at the same instant.
//!
//! Run: cargo test --test sensors -- --test-threads=1

use crate::common::sensor_lifecycle as sl;
use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;
use uuid::Uuid;

/// A stream owned by `sensor_id` and paired to nothing: its readings carry the instrument but no
/// site, which is the state an adopt exists to resolve.
async fn seed_unadopted_stream(
    db: &sea_orm::DatabaseConnection,
    source_key: &str,
    sensor_id: Uuid,
) -> Uuid {
    let stream = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active, sensor_id) \
             VALUES ('{stream}', 'test', '{source_key}', 'Imp {source_key}', true, '{sensor_id}')"
        ),
    )
    .await;
    for i in 0..6 {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO readings (stream_id, time, raw_value, replicate_index, sensor_id) \
                 VALUES ('{stream}', '2025-06-01T00:{:02}:00Z', {}, 0, '{sensor_id}')",
                i * 10,
                10.0 + f64::from(i)
            ),
        )
        .await;
    }
    stream
}

#[tokio::test]
#[serial]
async fn adopt_backfills_by_window() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let site1 = Uuid::parse_str(crate::common::SITE1_ID).unwrap();
    let temp = Uuid::parse_str(crate::common::GLOBAL_PARAM_TEMP_ID).unwrap();

    let sensor =
        sl::create_sensor(&db, "adopt-backfill", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let sensor_id = sensor.id;
    let stream = seed_unadopted_stream(&db, "adopt-backfill", sensor_id).await;

    let rows = sl::get_readings(&db, stream).await;
    assert_eq!(rows.len(), 6);
    for r in &rows {
        assert!(r.sensor_id.is_some(), "the instrument owns its readings");
        assert_eq!(r.site_id, None, "nothing is attributed to a site yet");
        assert_eq!(r.deployment_id, None, "nothing is deployed yet");
    }

    // ADOPT: deploy from before the first reading -> reprocess backfills site + deployment + parameter.
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
    assert_eq!(
        crate::common::e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint FROM reading_decisions \
                 WHERE stream_id = '{stream}' AND kind = 'attribution' \
                   AND old = '{{\"parameter_id\": null}}' \
                   AND new = '{{\"parameter_id\": \"{temp}\"}}'"
            ),
        )
        .await,
        6,
        "each reading the adopt gave a parameter records the claim"
    );
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

/// Scenario: an instrument open at site 1 since March is adopted at site 2 for January and
/// February, a history recorded after the fact.
/// Expected behaviour: the March deployment is left as it is, since it began after the adopt's
/// start, and the backdated window is stored beside it.
#[tokio::test]
#[serial]
async fn adopt_backdated_before_a_later_open_deployment_keeps_it() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor = sl::create_sensor(&db, "backdated", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let later = sl::deploy_sensor(
        &db,
        sensor.id,
        crate::common::SITE1_ID,
        sl::dt("2026-03-01T00:00:00Z"),
    )
    .await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sensors/{}/adopt", sensor.id),
        &serde_json::json!({
            "site_id": crate::common::SITE2_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
            "deployed_from": "2026-01-01T00:00:00Z",
            "deployed_until": "2026-03-01T00:00:00Z"
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "adopt ({status}): {body}");

    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT deployed_until::text AS u FROM sensor_deployments WHERE id = $1",
            [later.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    let until: Option<String> = row.try_get("", "u").unwrap();
    assert_eq!(until, None, "the March deployment stays open");
    let windows: i64 = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*) AS c FROM sensor_deployments WHERE sensor_id = $1",
            [sensor.id.into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "c")
        .unwrap();
    assert_eq!(windows, 2, "the backdated window is stored beside it");
}

/// Scenario: a swap names an instant before the outgoing instrument's deployment began.
/// Expected behaviour: the outgoing row is not ended before its own start, and the swap is refused
/// as a slot conflict rather than a database error.
#[tokio::test]
#[serial]
async fn swap_before_the_outgoing_start_is_a_conflict() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let sensor_a = sl::create_sensor(&db, "early-a", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let sensor_b = sl::create_sensor(&db, "early-b", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let outgoing = sl::deploy_sensor(
        &db,
        sensor_a.id,
        crate::common::SITE1_ID,
        sl::dt("2026-03-01T00:00:00Z"),
    )
    .await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/actions/swap",
        &serde_json::json!({
            "outgoing_sensor_id": sensor_a.id,
            "incoming_sensor_id": sensor_b.id,
            "site_id": crate::common::SITE1_ID,
            "at": "2026-02-01T00:00:00Z"
        }),
        &token,
    )
    .await;
    assert_eq!(
        status, 409,
        "swap before the outgoing start ({status}): {body}"
    );

    let until: Option<String> = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT deployed_until::text AS u FROM sensor_deployments WHERE id = $1",
            [outgoing.into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "u")
        .unwrap();
    assert_eq!(until, None, "the outgoing deployment is untouched");
}

/// Deployments an instrument holds, and how many of them are open.
async fn deployments_of(db: &sea_orm::DatabaseConnection, sensor_id: Uuid) -> (i64, i64) {
    let total = crate::common::e2e::count(
        db,
        &format!("SELECT COUNT(*)::bigint FROM sensor_deployments WHERE sensor_id = '{sensor_id}'"),
    )
    .await;
    let open = crate::common::e2e::count(
        db,
        &format!(
            "SELECT COUNT(*)::bigint FROM sensor_deployments \
             WHERE sensor_id = '{sensor_id}' AND deployed_until IS NULL"
        ),
    )
    .await;
    (total, open)
}

/// Scenario: the adopt's reprocess cannot be queued. Expected behaviour: the adopt is refused
/// whole, so no deployment is left standing whose history nothing will ever attribute.
#[tokio::test]
#[serial]
async fn an_adopt_whose_reprocess_cannot_be_queued_deploys_nothing() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let sensor = sl::create_sensor(&db, "adopt-lost", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    seed_unadopted_stream(&db, "adopt-lost", sensor.id).await;

    crate::common::jobs::refuse_enqueue(&db, "manual_adopt").await;
    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sensors/{}/adopt", sensor.id),
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
            "deployed_from": "2025-05-01T00:00:00Z"
        }),
        &token,
    )
    .await;
    crate::common::jobs::restore_enqueue(&db).await;

    assert_eq!(status, 500, "the adopt reports the failure: {body}");
    assert_eq!(
        deployments_of(&db, sensor.id).await,
        (0, 0),
        "the deployment and its reprocess commit together or not at all"
    );
}

/// Scenario: the swap's handover reprocess cannot be queued. Expected behaviour: the swap is
/// refused whole, so the outgoing instrument keeps the slot and the incoming one holds nothing.
#[tokio::test]
#[serial]
async fn a_swap_whose_reprocess_cannot_be_queued_changes_nothing() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let outgoing = sl::create_sensor(&db, "swap-lost-a", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    let incoming = sl::create_sensor(&db, "swap-lost-b", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    sl::deploy_sensor(
        &db,
        outgoing.id,
        crate::common::SITE1_ID,
        sl::dt("2025-06-01T00:00:00Z"),
    )
    .await;

    crate::common::jobs::refuse_enqueue(&db, "sensor_swap").await;
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/actions/swap",
        &serde_json::json!({
            "outgoing_sensor_id": outgoing.id,
            "incoming_sensor_id": incoming.id,
            "site_id": crate::common::SITE1_ID,
            "at": "2025-06-01T00:30:00Z"
        }),
        &token,
    )
    .await;
    crate::common::jobs::restore_enqueue(&db).await;

    assert_eq!(status, 500, "the swap reports the failure: {body}");
    assert_eq!(
        deployments_of(&db, outgoing.id).await,
        (1, 1),
        "the outgoing instrument still holds the slot"
    );
    assert_eq!(
        deployments_of(&db, incoming.id).await,
        (0, 0),
        "the incoming instrument was never deployed"
    );
}
