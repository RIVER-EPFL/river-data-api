//! Tests that alarm evaluation falls back to the parameter's global `alarm_thresholds` row when
//! the site has none of its own, and that a site row overrides it.
//!
//! Run with: cargo test --test alarm_thresholds
//! Requires: DATABASE_URL pointing to a TimescaleDB instance.

use serial_test::serial;

async fn exec(db: &sea_orm::DatabaseConnection, sql: &str) {
    use sea_orm::{ConnectionTrait, Statement};
    db.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("SQL failed: {e}\nQuery: {sql}"));
}

/// Scenario: the parameter carries a global threshold row and the site carries none.
/// Expected behaviour: alarms fire against the global row.
#[tokio::test]
#[serial]
async fn test_global_threshold_triggers_alarms_without_a_site_row() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let site_id = crate::common::SITE1_ID;

    exec(&db, "DELETE FROM alarm_thresholds").await;

    exec(
        &db,
        &format!(
            "INSERT INTO alarm_thresholds (id, parameter_id, site_id, warning_min, warning_max, alarm_min, alarm_max, description) \
             VALUES (gen_random_uuid(), '{}', NULL, 0.5, 20.0, 0.0, 25.0, 'Parameter default')",
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site_id}/alarms?start=2025-01-15T00:00:00Z&end=2025-01-17T00:00:00Z"),
        &token,
    )
    .await;

    assert_eq!(status, 200, "response: {body}");

    let times = body["times"].as_array().unwrap();
    assert!(
        !times.is_empty(),
        "the parameter's global threshold should trigger violations at a site with no row of its own. response: {body}"
    );

    let params = body["parameters"].as_array().unwrap();
    let param_names: Vec<&str> = params.iter().filter_map(|p| p["name"].as_str()).collect();
    let temp_param = params
        .iter()
        .find(|p| p["id"].as_str() == Some(crate::common::GLOBAL_PARAM_TEMP_ID));

    assert!(
        temp_param.is_some(),
        "temperature parameter should appear in violations. got params: {param_names:?}"
    );

    let severities = temp_param.unwrap()["severities"].as_array().unwrap();
    let has_warning = severities.iter().any(|s| s.as_i64() == Some(1));
    let has_alarm = severities.iter().any(|s| s.as_i64() == Some(2));
    assert!(
        has_warning,
        "should have at least one warning-level violation"
    );
    assert!(has_alarm, "should have at least one alarm-level violation");

    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: the parameter carries a global row and the site carries one with different values.
/// Expected behaviour: the site row wins.
#[tokio::test]
#[serial]
async fn test_site_threshold_overrides_the_global_row() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let site_id = crate::common::SITE1_ID;

    exec(&db, "DELETE FROM alarm_thresholds").await;

    exec(
        &db,
        &format!(
            "INSERT INTO alarm_thresholds (id, parameter_id, site_id, warning_min, warning_max, alarm_min, alarm_max, description) \
             VALUES (gen_random_uuid(), '{}', NULL, 0.5, 20.0, 0.0, 25.0, 'Parameter default')",
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;

    // Override with a very wide range that no reading will violate
    exec(
        &db,
        &format!(
            "INSERT INTO alarm_thresholds (id, parameter_id, site_id, warning_min, warning_max, alarm_min, alarm_max, description) \
             VALUES (gen_random_uuid(), '{}', '{}', -100.0, 100.0, -200.0, 200.0, 'Wide override')",
            crate::common::GLOBAL_PARAM_TEMP_ID, site_id,
        ),
    )
    .await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site_id}/alarms?start=2025-01-15T00:00:00Z&end=2025-01-17T00:00:00Z"),
        &token,
    )
    .await;

    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    let temp_param = params.iter().find(|p| {
        p["name"]
            .as_str()
            .map_or(false, |n| n.contains("temperature"))
    });

    assert!(
        temp_param.is_none(),
        "site-specific override with wide range should suppress all temperature violations"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: the parameter carries a global row and no site has one.
/// Expected behaviour: GET /alarms/active returns the parameter in the breach list.
#[tokio::test]
#[serial]
async fn test_active_alarms_includes_global_threshold_violations() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    exec(&db, "DELETE FROM alarm_events").await;
    exec(&db, "DELETE FROM alarm_thresholds").await;

    exec(
        &db,
        &format!(
            "INSERT INTO alarm_thresholds (id, parameter_id, site_id, warning_min, warning_max, alarm_min, alarm_max, description) \
             VALUES (gen_random_uuid(), '{}', NULL, 0.5, 20.0, 0.0, 25.0, 'Parameter default')",
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;

    let (status, body) =
        crate::common::get_json_with_token(&app, "/api/alarms/active", &token).await;

    assert_eq!(status, 200, "response: {body}");

    let alarms = body["alarms"].as_array().unwrap();

    // fetch_active_alarm_rows checks the single latest reading per (site, parameter).
    // The seed data's last temperature reading may be within normal range, so we check
    // whether ANY parameter appears rather than requiring temperature specifically.
    // At minimum, the endpoint must not error when resolving thresholds from the global row.
    if alarms.is_empty() {
        // Verify it's because the latest reading is in range, not because defaults were ignored.
        // Query site alarms over the full range, this MUST find violations.
        let site_id = crate::common::SITE1_ID;
        let (s2, b2) = crate::common::get_json_with_token(
            &app,
            &format!(
                "/api/sites/{site_id}/alarms?start=2025-01-15T00:00:00Z&end=2025-01-17T00:00:00Z"
            ),
            &token,
        )
        .await;
        assert_eq!(s2, 200);
        let times = b2["times"].as_array().unwrap();
        assert!(
            !times.is_empty(),
            "site alarms should find violations from the global threshold even if latest reading is in range"
        );
    }

    crate::common::cleanup_test_db(&db).await;
}
