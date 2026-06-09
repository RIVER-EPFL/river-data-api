//! Tests for calibration & deployment reprocessing.
//!
//! Every test uses known raw_values and verifies exact calibrated outputs.
//! Each reading has a distinct raw_value so we can trace which specific
//! reading was affected by a calibration or deployment change.
//!
//! Run: DATABASE_URL=postgresql://postgres:psql@localhost:5444/river_test \
//!      cargo test --test sensor_calibrations -- --test-threads=1


use crate::common::sensor_lifecycle::*;
use crate::common::*;
use sea_orm::ConnectionTrait;
use serial_test::serial;
use std::time::Duration;

const WAIT_TIMEOUT: Duration = Duration::from_secs(5);

// ============================================================================
// Baseline: verify the current ingestion path works
// ============================================================================

/// Identity calibration (slope=1, intercept=0) preserves raw values.
/// All FK columns are stamped correctly at ingestion time.
///
/// Input:  raw = [20.0, 21.5, 19.8]
/// Cal:    slope=1, intercept=0
/// Expect: calibrated = [20.0, 21.5, 19.8] (unchanged)
#[tokio::test]
#[serial]
async fn baseline_identity_calibration_preserves_raw_values() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Probe-001", GLOBAL_PARAM_TEMP_ID).await;
    let dep = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    let stream = create_paired_stream(&db, "baseline-probe", PARAM_S1_TEMP_ID).await;

    insert_readings(
        &db, stream, SITE1_ID, GLOBAL_PARAM_TEMP_ID,
        sensor.id, sensor.identity_calibration_id, dep,
        1.0, 0.0,
        &[
            (dt("2025-01-01T10:00:00Z"), 20.0),
            (dt("2025-01-01T10:10:00Z"), 21.5),
            (dt("2025-01-01T10:20:00Z"), 19.8),
        ],
    )
    .await;

    let rows = get_readings(&db, stream).await;
    assert_eq!(rows.len(), 3);

    let site1: uuid::Uuid = SITE1_ID.parse().unwrap();
    let param_temp: uuid::Uuid = GLOBAL_PARAM_TEMP_ID.parse().unwrap();

    // Each reading: calibrated = 1.0 * raw + 0.0 = raw
    for (i, (expected_raw, expected_cal)) in
        [(20.0, 20.0), (21.5, 21.5), (19.8, 19.8)].iter().enumerate()
    {
        assert_eq!(rows[i].raw_value, *expected_raw, "row {i} raw");
        assert_eq!(rows[i].calibrated_value, Some(*expected_cal), "row {i} calibrated");
        assert_eq!(rows[i].sensor_id, Some(sensor.id), "row {i} sensor_id");
        assert_eq!(rows[i].calibration_id, Some(sensor.identity_calibration_id), "row {i} cal_id");
        assert_eq!(rows[i].deployment_id, Some(dep), "row {i} dep_id");
        assert_eq!(rows[i].site_id, Some(site1), "row {i} site_id");
        assert_eq!(rows[i].parameter_id, Some(param_temp), "row {i} param_id");
    }

    cleanup_test_db(&db).await;
}

// ============================================================================
// Calibration value changes
// ============================================================================

/// Creating a new calibration recalculates all readings.
/// Verified both in the DB and through the API endpoint.
///
/// Input:  raw = [20.0, 21.0, 22.0]
/// Before: identity → calibrated = [20.0, 21.0, 22.0]
/// After:  slope=2, intercept=1 → calibrated = [41.0, 43.0, 45.0]
#[tokio::test]
#[serial]
async fn recalibration_updates_all_readings() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Probe-001", GLOBAL_PARAM_TEMP_ID).await;
    let dep = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    let stream = create_paired_stream(&db, "recal-probe", PARAM_S1_TEMP_ID).await;

    let raw_values = [20.0, 21.0, 22.0];
    let times = [
        dt("2025-01-01T10:00:00Z"),
        dt("2025-01-01T10:10:00Z"),
        dt("2025-01-01T10:20:00Z"),
    ];
    let readings: Vec<_> = times.iter().zip(raw_values.iter()).map(|(t, v)| (*t, *v)).collect();

    insert_readings(
        &db, stream, SITE1_ID, GLOBAL_PARAM_TEMP_ID,
        sensor.id, sensor.identity_calibration_id, dep,
        1.0, 0.0, &readings,
    )
    .await;

    // Verify BEFORE state
    let rows = get_readings(&db, stream).await;
    for (i, raw) in raw_values.iter().enumerate() {
        assert_eq!(rows[i].calibrated_value, Some(*raw), "before: row {i} = identity");
    }

    // Apply new calibration: calibrated = 2 * raw + 1
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;
    let (status, body) = post_json_with_token(
        &app,
        "/api/sensor_calibrations",
        &serde_json::json!({
            "sensor_id": sensor.id,
            "slope": 2.0,
            "intercept": 1.0,
            "valid_from": "2025-01-01T00:00:00Z"
        }),
        &token,
    )
    .await;
    assert_eq!(status, 201, "create calibration: {body}");
    let real_cal: uuid::Uuid =
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str().unwrap().parse().unwrap();

    assert!(wait_for_reprocessing(&db, sensor.id, WAIT_TIMEOUT).await);

    // Verify AFTER state — DB level: each reading individually
    let rows = get_readings(&db, stream).await;
    assert_eq!(rows.len(), 3);
    for (i, raw) in raw_values.iter().enumerate() {
        let expected = 2.0 * raw + 1.0;
        assert_eq!(rows[i].raw_value, *raw, "row {i} raw unchanged");
        assert_eq!(rows[i].calibrated_value, Some(expected), "row {i}: 2*{raw}+1 = {expected}");
        assert_eq!(rows[i].calibration_id, Some(real_cal), "row {i} cal FK updated");
    }

    // Verify AFTER state — API level: GET /sites/{id}/readings returns calibrated values
    let (status, json) = get_json_with_token(
        &app,
        &format!(
            "/api/sites/{}/readings?start=2025-01-01T00:00:00Z&end=2025-01-02T00:00:00Z\
             &sensor_types=DO_Temperature",
            SITE1_ID
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    let params = json["parameters"].as_array().unwrap();
    let temp_param = params
        .iter()
        .find(|p| p["name"].as_str() == Some("DO_Temperature"))
        .expect("DO_Temperature parameter should be in response");
    let values = temp_param["values"].as_array().unwrap();
    assert_eq!(values.len(), 3, "3 readings returned via API");
    for (i, raw) in raw_values.iter().enumerate() {
        let expected = 2.0 * raw + 1.0;
        let api_val = values[i].as_f64().unwrap();
        assert!(
            (api_val - expected).abs() < 0.001,
            "API row {i}: expected {expected}, got {api_val}"
        );
    }

    cleanup_test_db(&db).await;
}

// ============================================================================
// Deployment changes — site_id and reading count
// ============================================================================

/// Sensor moves from site 1 to site 2 at 12:00.
/// Readings before 12:00 keep site 1; readings from 12:00 get site 2.
/// Verified: deployment_id, site_id, and reading counts per site.
///
/// 6 readings: 10:00-15:00 hourly
/// Cutoff: 12:00
/// Expected: 2 readings at site 1, 4 readings at site 2
#[tokio::test]
#[serial]
async fn deployment_change_updates_site_and_deployment() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Probe-001", GLOBAL_PARAM_TEMP_ID).await;
    let dep_a = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    let stream = create_paired_stream(&db, "transfer-probe", PARAM_S1_TEMP_ID).await;

    let site1: uuid::Uuid = SITE1_ID.parse().unwrap();
    let site2: uuid::Uuid = SITE2_ID.parse().unwrap();

    // 6 readings with distinct values so each is identifiable
    insert_readings(
        &db, stream, SITE1_ID, GLOBAL_PARAM_TEMP_ID,
        sensor.id, sensor.identity_calibration_id, dep_a,
        1.0, 0.0,
        &[
            (dt("2025-01-01T10:00:00Z"), 10.0),
            (dt("2025-01-01T11:00:00Z"), 11.0),
            (dt("2025-01-01T12:00:00Z"), 12.0),
            (dt("2025-01-01T13:00:00Z"), 13.0),
            (dt("2025-01-01T14:00:00Z"), 14.0),
            (dt("2025-01-01T15:00:00Z"), 15.0),
        ],
    )
    .await;

    // BEFORE: all 6 readings at site 1
    let rows = get_readings(&db, stream).await;
    assert_eq!(rows.iter().filter(|r| r.site_id == Some(site1)).count(), 6, "before: 6 at site 1");
    assert_eq!(rows.iter().filter(|r| r.site_id == Some(site2)).count(), 0, "before: 0 at site 2");

    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    // End deployment A at 12:00
    let (status, _) = put_json_with_token(
        &app,
        &format!("/api/sensor_deployments/{dep_a}"),
        &serde_json::json!({ "deployed_until": "2025-01-01T12:00:00Z" }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert!(wait_for_reprocessing(&db, sensor.id, WAIT_TIMEOUT).await);

    // Create deployment B at site 2 from 12:00
    let (status, body) = post_json_with_token(
        &app,
        "/api/sensor_deployments",
        &serde_json::json!({
            "sensor_id": sensor.id,
            "site_id": SITE2_ID,
            "deployed_from": "2025-01-01T12:00:00Z",
            "deployment_type": "permanent"
        }),
        &token,
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let dep_b: uuid::Uuid =
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str().unwrap().parse().unwrap();

    assert!(wait_for_reprocessing(&db, sensor.id, WAIT_TIMEOUT).await);

    let rows = get_readings(&db, stream).await;
    assert_eq!(rows.len(), 6, "total readings unchanged");

    // AFTER: reading counts per site
    let at_site1 = rows.iter().filter(|r| r.site_id == Some(site1)).count();
    let at_site2 = rows.iter().filter(|r| r.site_id == Some(site2)).count();
    assert_eq!(at_site1, 2, "2 readings remain at site 1 (10:00, 11:00)");
    assert_eq!(at_site2, 4, "4 readings moved to site 2 (12:00-15:00)");

    // AFTER: each reading individually — deployment_id + site_id + value intact
    let expected: [(f64, uuid::Uuid, uuid::Uuid); 6] = [
        (10.0, dep_a, site1), // 10:00
        (11.0, dep_a, site1), // 11:00
        (12.0, dep_b, site2), // 12:00
        (13.0, dep_b, site2), // 13:00
        (14.0, dep_b, site2), // 14:00
        (15.0, dep_b, site2), // 15:00
    ];
    for (i, (raw, dep, site)) in expected.iter().enumerate() {
        assert_eq!(rows[i].raw_value, *raw, "row {i} raw");
        assert_eq!(rows[i].calibrated_value, Some(*raw), "row {i} cal (identity)");
        assert_eq!(rows[i].deployment_id, Some(*dep), "row {i} deployment");
        assert_eq!(rows[i].site_id, Some(*site), "row {i} site");
    }

    cleanup_test_db(&db).await;
}

// ============================================================================
// Calibration date changes — distinct values prove which readings moved
// ============================================================================

/// Real calibration's valid_from moves from Jan 15 to Jan 12.
/// Each reading has a distinct raw_value (= day number) so we can verify
/// exactly which readings switched from identity to real calibration.
///
/// Identity:   calibrated = raw (slope=1, intercept=0)
/// Real cal:   calibrated = 3*raw + 0.5
///
/// Before move (cal starts Jan 15):
///   Jan 10 (raw=10) → identity → 10.0
///   Jan 11 (raw=11) → identity → 11.0
///   Jan 12 (raw=12) → identity → 12.0      ← these 3 will switch
///   Jan 13 (raw=13) → identity → 13.0
///   Jan 14 (raw=14) → identity → 14.0
///   Jan 15 (raw=15) → real     → 45.5
///   Jan 16 (raw=16) → real     → 48.5
///
/// After move (cal starts Jan 12):
///   Jan 10 (raw=10) → identity → 10.0
///   Jan 11 (raw=11) → identity → 11.0
///   Jan 12 (raw=12) → real     → 36.5      ← changed
///   Jan 13 (raw=13) → real     → 39.5      ← changed
///   Jan 14 (raw=14) → real     → 42.5      ← changed
///   Jan 15 (raw=15) → real     → 45.5
///   Jan 16 (raw=16) → real     → 48.5
#[tokio::test]
#[serial]
async fn retroactive_calibration_date_change() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Probe-001", GLOBAL_PARAM_TEMP_ID).await;
    let dep = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    let stream = create_paired_stream(&db, "date-change-probe", PARAM_S1_TEMP_ID).await;

    let real_cal = add_calibration(&db, sensor.id, 3.0, 0.5, dt("2025-01-15T00:00:00Z")).await;

    // Jan 10-14: ingested with identity calibration, raw = day number
    let identity_readings: Vec<_> = (10..15)
        .map(|d| (dt(&format!("2025-01-{d:02}T12:00:00Z")), d as f64))
        .collect();
    // Jan 15-16: ingested with real calibration, raw = day number
    let real_readings: Vec<_> = (15..17)
        .map(|d| (dt(&format!("2025-01-{d:02}T12:00:00Z")), d as f64))
        .collect();

    insert_readings(
        &db, stream, SITE1_ID, GLOBAL_PARAM_TEMP_ID,
        sensor.id, sensor.identity_calibration_id, dep,
        1.0, 0.0, &identity_readings,
    )
    .await;
    insert_readings(
        &db, stream, SITE1_ID, GLOBAL_PARAM_TEMP_ID,
        sensor.id, real_cal, dep,
        3.0, 0.5, &real_readings,
    )
    .await;

    // Verify BEFORE state: 5 under identity, 2 under real cal
    let rows = get_readings(&db, stream).await;
    assert_eq!(rows.len(), 7);
    let identity_count = rows.iter().filter(|r| r.calibration_id == Some(sensor.identity_calibration_id)).count();
    let real_count = rows.iter().filter(|r| r.calibration_id == Some(real_cal)).count();
    assert_eq!(identity_count, 5, "before: 5 readings under identity (Jan 10-14)");
    assert_eq!(real_count, 2, "before: 2 readings under real cal (Jan 15-16)");

    // Move valid_from from Jan 15 → Jan 12
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;
    let (status, _) = put_json_with_token(
        &app,
        &format!("/api/sensor_calibrations/{real_cal}"),
        &serde_json::json!({ "valid_from": "2025-01-12T00:00:00Z" }),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert!(wait_for_reprocessing(&db, sensor.id, WAIT_TIMEOUT).await);

    // Verify AFTER state: counts shifted
    let rows = get_readings(&db, stream).await;
    let identity_count = rows.iter().filter(|r| r.calibration_id == Some(sensor.identity_calibration_id)).count();
    let real_count = rows.iter().filter(|r| r.calibration_id == Some(real_cal)).count();
    assert_eq!(identity_count, 2, "after: 2 readings under identity (Jan 10-11)");
    assert_eq!(real_count, 5, "after: 5 readings under real cal (Jan 12-16)");

    // Verify AFTER state: each reading's exact calibrated value
    //   Identity readings: calibrated = 1*raw + 0 = raw
    //   Real cal readings: calibrated = 3*raw + 0.5
    let expected: [(f64, f64, uuid::Uuid); 7] = [
        (10.0, 10.0, sensor.identity_calibration_id),  // Jan 10, identity
        (11.0, 11.0, sensor.identity_calibration_id),  // Jan 11, identity
        (12.0, 36.5, real_cal),                         // Jan 12, SWITCHED: 3*12+0.5
        (13.0, 39.5, real_cal),                         // Jan 13, SWITCHED: 3*13+0.5
        (14.0, 42.5, real_cal),                         // Jan 14, SWITCHED: 3*14+0.5
        (15.0, 45.5, real_cal),                         // Jan 15, unchanged: 3*15+0.5
        (16.0, 48.5, real_cal),                         // Jan 16, unchanged: 3*16+0.5
    ];
    for (i, (raw, cal, cal_id)) in expected.iter().enumerate() {
        assert_eq!(rows[i].raw_value, *raw, "row {i} raw");
        assert_eq!(rows[i].calibrated_value, Some(*cal), "row {i}: raw={raw} → cal={cal}");
        assert_eq!(rows[i].calibration_id, Some(*cal_id), "row {i} cal FK");
    }

    cleanup_test_db(&db).await;
}

// ============================================================================
// Lab sensor — multi-site with site_id verification
// ============================================================================

/// Lab sensor deployed at 3 sites in one day. Each reading has a distinct
/// raw_value. After deployment creation, verify both deployment_id and
/// site_id, plus counts per site.
///
/// dep_a: SITE1 08:00-10:00 → readings at 09:00 (raw=9), 09:30 (raw=9.5)
/// dep_b: SITE2 10:00-12:00 → readings at 10:30 (raw=10.5), 11:00 (raw=11)
/// dep_c: SITE1 12:00-14:00 → readings at 12:30 (raw=12.5), 13:00 (raw=13)
///
/// Counts: 4 at SITE1 (dep_a + dep_c), 2 at SITE2 (dep_b)
#[tokio::test]
#[serial]
async fn lab_sensor_multiple_deployments_same_day() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Lab-Probe-001", GLOBAL_PARAM_TEMP_ID).await;
    let site1: uuid::Uuid = SITE1_ID.parse().unwrap();
    let site2: uuid::Uuid = SITE2_ID.parse().unwrap();

    let dep_a = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-06-15T08:00:00Z")).await;
    end_deployment(&db, dep_a, dt("2025-06-15T10:00:00Z")).await;

    let stream = create_paired_stream(&db, "lab-probe", PARAM_S1_TEMP_ID).await;

    // All readings initially stamped with dep_a — distinct raw values
    insert_readings(
        &db, stream, SITE1_ID, GLOBAL_PARAM_TEMP_ID,
        sensor.id, sensor.identity_calibration_id, dep_a,
        1.0, 0.0,
        &[
            (dt("2025-06-15T09:00:00Z"), 9.0),
            (dt("2025-06-15T09:30:00Z"), 9.5),
            (dt("2025-06-15T10:30:00Z"), 10.5),
            (dt("2025-06-15T11:00:00Z"), 11.0),
            (dt("2025-06-15T12:30:00Z"), 12.5),
            (dt("2025-06-15T13:00:00Z"), 13.0),
        ],
    )
    .await;

    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    // Create dep_b: SITE2 10:00-12:00
    let (status, body) = post_json_with_token(
        &app,
        "/api/sensor_deployments",
        &serde_json::json!({
            "sensor_id": sensor.id,
            "site_id": SITE2_ID,
            "deployed_from": "2025-06-15T10:00:00Z",
            "deployed_until": "2025-06-15T12:00:00Z",
            "deployment_type": "field_campaign"
        }),
        &token,
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let dep_b: uuid::Uuid = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str().unwrap().parse().unwrap();
    assert!(wait_for_reprocessing(&db, sensor.id, WAIT_TIMEOUT).await);

    // Create dep_c: SITE1 12:00-14:00
    let (status, body) = post_json_with_token(
        &app,
        "/api/sensor_deployments",
        &serde_json::json!({
            "sensor_id": sensor.id,
            "site_id": SITE1_ID,
            "deployed_from": "2025-06-15T12:00:00Z",
            "deployed_until": "2025-06-15T14:00:00Z",
            "deployment_type": "field_campaign"
        }),
        &token,
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let dep_c: uuid::Uuid = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str().unwrap().parse().unwrap();
    assert!(wait_for_reprocessing(&db, sensor.id, WAIT_TIMEOUT).await);

    let rows = get_readings(&db, stream).await;
    assert_eq!(rows.len(), 6);

    // Count per site
    let at_site1 = rows.iter().filter(|r| r.site_id == Some(site1)).count();
    let at_site2 = rows.iter().filter(|r| r.site_id == Some(site2)).count();
    assert_eq!(at_site1, 4, "4 readings at site 1 (dep_a + dep_c)");
    assert_eq!(at_site2, 2, "2 readings at site 2 (dep_b)");

    // Each reading: raw_value, deployment, site
    let expected: [(f64, uuid::Uuid, uuid::Uuid); 6] = [
        (9.0,  dep_a, site1),  // 09:00
        (9.5,  dep_a, site1),  // 09:30
        (10.5, dep_b, site2),  // 10:30
        (11.0, dep_b, site2),  // 11:00
        (12.5, dep_c, site1),  // 12:30
        (13.0, dep_c, site1),  // 13:00
    ];
    for (i, (raw, dep, site)) in expected.iter().enumerate() {
        assert_eq!(rows[i].raw_value, *raw, "row {i} raw");
        assert_eq!(rows[i].deployment_id, Some(*dep), "row {i} deployment");
        assert_eq!(rows[i].site_id, Some(*site), "row {i} site");
    }

    cleanup_test_db(&db).await;
}

// ============================================================================
// valid_until auto-management
// ============================================================================

/// Creating a new calibration auto-sets valid_until on the previous one.
#[tokio::test]
#[serial]
async fn calibration_time_windows_auto_bounded() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Probe-001", GLOBAL_PARAM_TEMP_ID).await;

    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;
    let (status, _) = post_json_with_token(
        &app,
        "/api/sensor_calibrations",
        &serde_json::json!({
            "sensor_id": sensor.id,
            "slope": 2.0,
            "intercept": 0.0,
            "valid_from": "2025-01-15T00:00:00Z"
        }),
        &token,
    )
    .await;
    assert_eq!(status, 201);
    assert!(wait_for_reprocessing(&db, sensor.id, WAIT_TIMEOUT).await);

    let rows = db
        .query_all(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, valid_from, valid_until FROM sensor_calibrations \
             WHERE sensor_id = $1 ORDER BY valid_from",
            [sensor.id.into()],
        ))
        .await
        .expect("query calibrations failed");

    assert_eq!(rows.len(), 2);

    // Identity cal: valid_until auto-set to Jan 15
    let identity_until: Option<chrono::DateTime<chrono::FixedOffset>> =
        rows[0].try_get("", "valid_until").ok();
    assert_eq!(
        identity_until.map(|t| t.with_timezone(&chrono::Utc)),
        Some(dt("2025-01-15T00:00:00Z")),
        "identity cal should have valid_until = next cal's valid_from"
    );

    // New cal: open-ended
    let cal_b_until: Option<chrono::DateTime<chrono::FixedOffset>> =
        rows[1].try_get("", "valid_until").ok();
    assert!(cal_b_until.is_none(), "latest calibration should have NULL valid_until");

    cleanup_test_db(&db).await;
}

// ============================================================================
// Calibration deletion — fallback with distinct values
// ============================================================================

/// Three calibrations: identity → A → B. Delete A.
/// Readings in A's window fall back to identity.
/// Each reading has distinct raw_value, so exact outputs are verifiable.
///
/// Before delete:
///   Jan 5  (raw=5)  → identity  → 5.0     (1*5+0)
///   Jan 15 (raw=15) → cal_a     → 31.0    (2*15+1)
///   Jan 25 (raw=25) → cal_b     → 77.0    (3*25+2)
///
/// After deleting cal_a:
///   Jan 5  (raw=5)  → identity  → 5.0     (unchanged)
///   Jan 15 (raw=15) → identity  → 15.0    (CHANGED: was 31.0)
///   Jan 25 (raw=25) → cal_b     → 77.0    (unchanged)
#[tokio::test]
#[serial]
async fn delete_intermediate_calibration_fallback() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Probe-001", GLOBAL_PARAM_TEMP_ID).await;
    let dep = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    let stream = create_paired_stream(&db, "fallback-probe", PARAM_S1_TEMP_ID).await;

    let cal_a = add_calibration(&db, sensor.id, 2.0, 1.0, dt("2025-01-10T00:00:00Z")).await;
    let cal_b = add_calibration(&db, sensor.id, 3.0, 2.0, dt("2025-01-20T00:00:00Z")).await;

    // Distinct raw values = day number
    insert_readings(
        &db, stream, SITE1_ID, GLOBAL_PARAM_TEMP_ID,
        sensor.id, sensor.identity_calibration_id, dep,
        1.0, 0.0,
        &[(dt("2025-01-05T12:00:00Z"), 5.0)],
    ).await;
    insert_readings(
        &db, stream, SITE1_ID, GLOBAL_PARAM_TEMP_ID,
        sensor.id, cal_a, dep,
        2.0, 1.0,
        &[(dt("2025-01-15T12:00:00Z"), 15.0)],
    ).await;
    insert_readings(
        &db, stream, SITE1_ID, GLOBAL_PARAM_TEMP_ID,
        sensor.id, cal_b, dep,
        3.0, 2.0,
        &[(dt("2025-01-25T12:00:00Z"), 25.0)],
    ).await;

    // BEFORE: verify exact values
    let rows = get_readings(&db, stream).await;
    assert_eq!(rows[0].calibrated_value, Some(5.0), "before: 1*5+0=5");
    assert_eq!(rows[1].calibrated_value, Some(31.0), "before: 2*15+1=31");
    assert_eq!(rows[2].calibrated_value, Some(77.0), "before: 3*25+2=77");

    // Delete cal_a
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;
    let (status, _) = delete_with_token(
        &app,
        &format!("/api/sensor_calibrations/{cal_a}"),
        &token,
    )
    .await;
    assert_eq!(status, 204);
    assert!(wait_for_reprocessing(&db, sensor.id, WAIT_TIMEOUT).await);

    // AFTER: Jan 15 reading falls back to identity
    let rows = get_readings(&db, stream).await;
    assert_eq!(rows.len(), 3, "no readings lost");

    assert_eq!(rows[0].raw_value, 5.0);
    assert_eq!(rows[0].calibrated_value, Some(5.0), "after: Jan 5 unchanged (identity)");
    assert_eq!(rows[0].calibration_id, Some(sensor.identity_calibration_id));

    assert_eq!(rows[1].raw_value, 15.0);
    assert_eq!(rows[1].calibrated_value, Some(15.0), "after: Jan 15 CHANGED 31→15 (fell back to identity: 1*15+0)");
    assert_eq!(rows[1].calibration_id, Some(sensor.identity_calibration_id));

    assert_eq!(rows[2].raw_value, 25.0);
    assert_eq!(rows[2].calibrated_value, Some(77.0), "after: Jan 25 unchanged (cal_b: 3*25+2)");
    assert_eq!(rows[2].calibration_id, Some(cal_b));

    cleanup_test_db(&db).await;
}

// ============================================================================
// Full cascade — readings + aggregates
// ============================================================================

/// Calibration change recalculates readings AND refreshes hourly aggregate.
///
/// 12 readings at raw=10.0 each, 10-min intervals 10:00-11:50.
/// Before: identity → avg_value = 10.0
/// After:  slope=2, intercept=5 → calibrated=25.0 → avg_value = 25.0
#[tokio::test]
#[serial]
async fn full_cascade_calibration_to_aggregates() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Probe-001", GLOBAL_PARAM_TEMP_ID).await;
    let dep = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-01-15T00:00:00Z")).await;
    let stream = create_paired_stream(&db, "cascade-probe", PARAM_S1_TEMP_ID).await;

    let readings: Vec<_> = (0..12)
        .map(|i| (dt("2025-01-15T10:00:00Z") + chrono::Duration::minutes(i * 10), 10.0))
        .collect();
    insert_readings(
        &db, stream, SITE1_ID, GLOBAL_PARAM_TEMP_ID,
        sensor.id, sensor.identity_calibration_id, dep,
        1.0, 0.0, &readings,
    )
    .await;

    exec(&db, "CALL refresh_continuous_aggregate('readings_hourly', '2025-01-14', '2025-01-16')").await;

    let agg_before = db
        .query_one(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT avg_value FROM readings_hourly \
             WHERE site_id = $1 AND parameter_id = $2 \
             AND bucket >= '2025-01-15T10:00:00Z' AND bucket < '2025-01-15T11:00:00Z'",
            [SITE1_ID.parse::<uuid::Uuid>().unwrap().into(), GLOBAL_PARAM_TEMP_ID.parse::<uuid::Uuid>().unwrap().into()],
        ))
        .await.unwrap();
    let avg_before: f64 = agg_before.unwrap().try_get("", "avg_value").unwrap();
    assert!((avg_before - 10.0).abs() < 0.01, "before: hourly avg = {avg_before}, expected 10.0");

    // Apply calibration: calibrated = 2*10+5 = 25.0
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;
    let (status, body) = post_json_with_token(
        &app,
        "/api/sensor_calibrations",
        &serde_json::json!({
            "sensor_id": sensor.id,
            "slope": 2.0,
            "intercept": 5.0,
            "valid_from": "2025-01-15T00:00:00Z"
        }),
        &token,
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert!(wait_for_reprocessing(&db, sensor.id, WAIT_TIMEOUT).await);

    // Verify readings: all 12 should be 25.0
    let rows = get_readings(&db, stream).await;
    assert_eq!(rows.len(), 12);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row.raw_value, 10.0, "row {i} raw unchanged");
        assert_eq!(row.calibrated_value, Some(25.0), "row {i}: 2*10+5=25");
    }

    // Verify aggregate: avg should now be 25.0
    let agg_after = db
        .query_one(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT avg_value FROM readings_hourly \
             WHERE site_id = $1 AND parameter_id = $2 \
             AND bucket >= '2025-01-15T10:00:00Z' AND bucket < '2025-01-15T11:00:00Z'",
            [SITE1_ID.parse::<uuid::Uuid>().unwrap().into(), GLOBAL_PARAM_TEMP_ID.parse::<uuid::Uuid>().unwrap().into()],
        ))
        .await.unwrap();
    let avg_after: f64 = agg_after.unwrap().try_get("", "avg_value").unwrap();
    assert!((avg_after - 25.0).abs() < 0.01, "after: hourly avg = {avg_after}, expected 25.0");

    cleanup_test_db(&db).await;
}
