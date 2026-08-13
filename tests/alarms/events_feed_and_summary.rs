//! Tests for the persisted alarm-events feed (`GET /api/alarms/events`) and the
//! last warning/alarm timestamps surfaced on `GET /api/alarms/summary`.
//!
//! Run with: cargo test --test alarms
//! Requires: DATABASE_URL pointing to a TimescaleDB instance.

use serial_test::serial;

async fn exec(db: &sea_orm::DatabaseConnection, sql: &str) {
    use sea_orm::{ConnectionTrait, Statement};
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("SQL failed: {e}\nQuery: {sql}"));
}

const WARNING_SEEN_AT: &str = "2025-02-01T10:00:00Z";
const ALARM_SEEN_AT: &str = "2025-02-02T11:30:00Z";

/// Seed one resolved warning event and one open alarm event on SITE1 / DO_Temperature.
async fn seed_alarm_events(db: &sea_orm::DatabaseConnection) {
    let site_id = crate::common::SITE1_ID;
    let param_id = crate::common::GLOBAL_PARAM_TEMP_ID;

    exec(
        db,
        &format!(
            "INSERT INTO alarm_events \
             (id, site_id, parameter_id, severity, max_severity, started_at, value_at_start, \
              last_seen_at, last_value, resolved_at, resolved_value) \
             VALUES (gen_random_uuid(), '{site_id}', '{param_id}', 1, 1, \
                     '2025-02-01T09:00:00Z', 21.0, '{WARNING_SEEN_AT}', 21.5, \
                     '2025-02-01T12:00:00Z', 18.0)"
        ),
    )
    .await;

    exec(
        db,
        &format!(
            "INSERT INTO alarm_events \
             (id, site_id, parameter_id, severity, max_severity, started_at, value_at_start, \
              last_seen_at, last_value) \
             VALUES (gen_random_uuid(), '{site_id}', '{param_id}', 2, 2, \
                     '2025-02-02T10:00:00Z', 26.0, '{ALARM_SEEN_AT}', 27.0)"
        ),
    )
    .await;
}

#[tokio::test]
#[serial]
async fn test_alarm_events_feed_and_filters() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    seed_alarm_events(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, body) =
        crate::common::get_json_with_token(&app, "/api/alarms/events", &token).await;
    assert_eq!(status, 200);
    assert_eq!(body["total"].as_u64(), Some(2));
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    // Ordered by last_seen_at DESC: the alarm (Feb 2) comes before the warning (Feb 1).
    assert_eq!(events[0]["max_severity"].as_i64(), Some(2));
    assert_eq!(events[1]["max_severity"].as_i64(), Some(1));
    assert_eq!(events[0]["site_name"].as_str(), Some("Upstream Station"));
    assert_eq!(events[0]["parameter_name"].as_str(), Some("DO_Temperature"));

    let (status, body) =
        crate::common::get_json_with_token(&app, "/api/alarms/events?severity=2", &token).await;
    assert_eq!(status, 200);
    assert_eq!(body["total"].as_u64(), Some(1));
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["max_severity"].as_i64(), Some(2));

    let (status, body) =
        crate::common::get_json_with_token(&app, "/api/alarms/events?status=open", &token).await;
    assert_eq!(status, 200);
    assert_eq!(body["total"].as_u64(), Some(1));
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0]["resolved_at"].is_null());
    assert_eq!(events[0]["max_severity"].as_i64(), Some(2));

    let (status, body) =
        crate::common::get_json_with_token(&app, "/api/alarms/events?status=resolved", &token)
            .await;
    assert_eq!(status, 200);
    assert_eq!(body["total"].as_u64(), Some(1));
    let events = body["events"].as_array().unwrap();
    assert_eq!(events[0]["max_severity"].as_i64(), Some(1));
    assert!(!events[0]["resolved_at"].is_null());

    let site_id = crate::common::SITE1_ID;
    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/alarms/events?site_id={site_id}"),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["total"].as_u64(), Some(2));

    // A different (seeded) site has no events.
    let other_site = crate::common::SITE2_ID;
    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/alarms/events?site_id={other_site}"),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["total"].as_u64(), Some(0));

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn test_alarm_events_severity_filter_binds_to_max_severity() {
    // Scenario: an event that began as a warning then escalated to an alarm has
    // severity=1 (current) but max_severity=2 (peak). The severity filter binds to
    // max_severity, so ?severity=2 returns it and ?severity=1 does not.
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    let site_id = crate::common::SITE1_ID;
    let param_id = crate::common::GLOBAL_PARAM_TEMP_ID;
    exec(
        &db,
        &format!(
            "INSERT INTO alarm_events \
             (id, site_id, parameter_id, severity, max_severity, started_at, value_at_start, \
              last_seen_at, last_value) \
             VALUES (gen_random_uuid(), '{site_id}', '{param_id}', 1, 2, \
                     '2025-03-01T09:00:00Z', 26.0, '2025-03-01T10:00:00Z', 21.0)"
        ),
    )
    .await;

    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, body) =
        crate::common::get_json_with_token(&app, "/api/alarms/events?severity=2", &token).await;
    assert_eq!(status, 200);
    assert_eq!(body["total"].as_u64(), Some(1));
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["severity"].as_i64(), Some(1));
    assert_eq!(events[0]["max_severity"].as_i64(), Some(2));

    let (status, body) =
        crate::common::get_json_with_token(&app, "/api/alarms/events?severity=1", &token).await;
    assert_eq!(status, 200);
    assert_eq!(body["total"].as_u64(), Some(0));
    assert_eq!(body["events"].as_array().unwrap().len(), 0);

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn test_alarm_summary_includes_last_warning_and_alarm() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    seed_alarm_events(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let (status, body) =
        crate::common::get_json_with_token(&app, "/api/alarms/summary", &token).await;
    assert_eq!(status, 200);

    let by_site = body["by_site"].as_array().unwrap();
    let site1 = by_site
        .iter()
        .find(|s| s["site_id"].as_str() == Some(crate::common::SITE1_ID))
        .expect("SITE1 should appear in alarm summary");

    let warning = chrono::DateTime::parse_from_rfc3339(WARNING_SEEN_AT).unwrap();
    let alarm = chrono::DateTime::parse_from_rfc3339(ALARM_SEEN_AT).unwrap();

    let got_warning =
        chrono::DateTime::parse_from_rfc3339(site1["last_warning_at"].as_str().unwrap()).unwrap();
    let got_alarm =
        chrono::DateTime::parse_from_rfc3339(site1["last_alarm_at"].as_str().unwrap()).unwrap();

    assert_eq!(got_warning, warning);
    assert_eq!(got_alarm, alarm);

    crate::common::cleanup_test_db(&db).await;
}
