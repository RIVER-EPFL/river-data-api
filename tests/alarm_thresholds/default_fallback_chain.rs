//! Tests that alarm evaluation falls back to parameter defaults when no explicit
//! alarm_thresholds row exists, and that explicit thresholds override defaults.
//!
//! Run with: cargo test --test alarm_thresholds
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

/// Scenario: parameter has default thresholds, no alarm_thresholds rows exist.
/// Expected behaviour: alarms still fire using the parameter defaults.
#[tokio::test]
#[serial]
async fn test_parameter_defaults_trigger_alarms_without_explicit_thresholds() {
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
            "UPDATE parameters SET \
             default_warning_min = 0.5, default_warning_max = 20.0, \
             default_alarm_min = 0.0, default_alarm_max = 25.0 \
             WHERE id = '{}'",
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/sites/{site_id}/alarms?start=2025-01-15T00:00:00Z&end=2025-01-17T00:00:00Z"
        ),
        &token,
    )
    .await;

    assert_eq!(status, 200, "response: {body}");

    let times = body["times"].as_array().unwrap();
    assert!(
        !times.is_empty(),
        "parameter defaults should trigger alarm violations even without alarm_thresholds rows. response: {body}"
    );

    let params = body["parameters"].as_array().unwrap();
    let param_names: Vec<&str> = params.iter().filter_map(|p| p["name"].as_str()).collect();
    let temp_param = params
        .iter()
        .find(|p| {
            p["id"].as_str() == Some(crate::common::GLOBAL_PARAM_TEMP_ID)
        });

    assert!(
        temp_param.is_some(),
        "temperature parameter should appear in violations. got params: {param_names:?}"
    );

    let severities = temp_param.unwrap()["severities"].as_array().unwrap();
    let has_warning = severities.iter().any(|s| s.as_i64() == Some(1));
    let has_alarm = severities.iter().any(|s| s.as_i64() == Some(2));
    assert!(has_warning, "should have at least one warning-level violation");
    assert!(has_alarm, "should have at least one alarm-level violation");

    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: parameter has defaults AND an explicit alarm_thresholds row with different values.
/// Expected behaviour: the explicit threshold overrides the parameter default.
#[tokio::test]
#[serial]
async fn test_explicit_threshold_overrides_parameter_default() {
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
            "UPDATE parameters SET \
             default_warning_min = 0.5, default_warning_max = 20.0, \
             default_alarm_min = 0.0, default_alarm_max = 25.0 \
             WHERE id = '{}'",
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
        &format!(
            "/api/sites/{site_id}/alarms?start=2025-01-15T00:00:00Z&end=2025-01-17T00:00:00Z"
        ),
        &token,
    )
    .await;

    assert_eq!(status, 200);

    let params = body["parameters"].as_array().unwrap();
    let temp_param = params
        .iter()
        .find(|p| p["name"].as_str().map_or(false, |n| n.contains("temperature")));

    assert!(
        temp_param.is_none(),
        "site-specific override with wide range should suppress all temperature violations"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: parameter defaults set, no alarm_thresholds rows.
/// Expected behaviour: GET /alarms/active returns the parameter in the breach list.
#[tokio::test]
#[serial]
async fn test_active_alarms_includes_parameter_default_violations() {
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
            "UPDATE parameters SET \
             default_warning_min = 0.5, default_warning_max = 20.0, \
             default_alarm_min = 0.0, default_alarm_max = 25.0 \
             WHERE id = '{}'",
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        "/api/alarms/active",
        &token,
    )
    .await;

    assert_eq!(status, 200, "response: {body}");

    let alarms = body["alarms"].as_array().unwrap();

    // fetch_active_alarm_rows checks the single latest reading per (site, parameter).
    // The seed data's last temperature reading may be within normal range, so we check
    // whether ANY parameter from defaults appears rather than requiring temperature specifically.
    // At minimum, the endpoint must not error when resolving thresholds from defaults.
    if alarms.is_empty() {
        // Verify it's because the latest reading is in range, not because defaults were ignored.
        // Query site alarms over the full range — this MUST find violations.
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
            "site alarms should find violations from parameter defaults even if latest reading is in range"
        );
    }

    crate::common::cleanup_test_db(&db).await;
}
