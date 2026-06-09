//! Regression: alarm thresholds are NOT auto-created from parameter defaults. Evaluation already
//! falls back to the parameter's `default_*` columns when no `alarm_thresholds` row exists, so a
//! default-valued row is redundant — and a site-specific copy would silently shadow a global
//! threshold an operator set (the cause of "the alarm I clicked shows nothing on the chart").
//!
//! Run: cargo test --test alarm_thresholds -- --test-threads=1


use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;

async fn threshold_count(db: &sea_orm::DatabaseConnection, param: &str, site: &str) -> i64 {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT count(*) AS c FROM alarm_thresholds WHERE parameter_id='{param}' AND site_id='{site}'"
        ),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "c")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn site_parameter_create_does_not_auto_create_or_shadow_threshold() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let depth = crate::common::GLOBAL_PARAM_DEPTH_ID;
    let site2 = crate::common::SITE2_ID; // seed does not give SITE2 a Depth site_parameter

    // Give Depth parameter defaults — the exact condition the (now-removed) hook fired on. The seed
    // already provides a global Depth threshold, so a correct create must leave that global in force.
    crate::common::exec(
        &db,
        &format!("UPDATE parameters SET default_warning_max = 999, default_alarm_max = 1000 WHERE id = '{depth}'"),
    )
    .await;

    // Creating a Depth site_parameter at SITE2 must NOT manufacture a site-specific row.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": site2,
            "parameter_id": depth,
            "name": "Depth",
            "sensor_type": "Depth",
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create site_parameter ({status}): {body}");

    assert_eq!(
        threshold_count(&db, depth, site2).await,
        0,
        "no site-specific threshold should be auto-created — the global (alarm > 500) must keep applying"
    );
}
