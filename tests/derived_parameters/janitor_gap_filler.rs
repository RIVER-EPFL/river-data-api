//! Scenario: source readings exist but no derived counterparts (simulating a
//! crash mid-batch, or a derived parameter assigned after data was ingested).
//!
//! Expected behaviour: invoking the janitor walks source readings and computes
//! any missing derived values.

use chrono::{DateTime, Duration, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

async fn count_derived(db: &DatabaseConnection, site_id: Uuid, parameter_id: Uuid) -> i64 {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COUNT(*) AS n FROM readings WHERE site_id = $1 AND parameter_id = $2",
            [site_id.into(), parameter_id.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    row.try_get::<i64>("", "n").unwrap()
}

async fn derived_value_at(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
    time: DateTime<Utc>,
) -> Option<f64> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COALESCE(calibrated_value, raw_value) AS value FROM readings \
             WHERE site_id = $1 AND parameter_id = $2 AND time = $3 LIMIT 1",
            [site_id.into(), parameter_id.into(), time.into()],
        ))
        .await
        .ok()
        .flatten()?;
    row.try_get::<f64>("", "value").ok()
}

#[tokio::test]
#[serial]
async fn test_janitor_fills_derived_gaps() {
    let (db, app, token) = setup().await;
    let site_id = Uuid::parse_str(crate::common::SITE1_ID).unwrap();

    let derived_name = format!("janitor_test_{}", Uuid::new_v4().simple());
    let create_body = serde_json::json!({
        "code": derived_name,
        "name": "Janitor test mg/L",
        "units": "mg/L",
        "formula": "Dissolved_O2 * 0.032",
    });
    let (status, def_json) = crate::common::post_json_parse_with_token(
        &app,
        "/api/derived_parameters",
        &create_body,
        &token,
    )
    .await;
    assert!((200..300).contains(&status));
    let derived_def_id = def_json["id"].as_str().unwrap().to_string();
    let output_parameter_id = def_json["output_parameter_id"]
        .as_str()
        .unwrap()
        .to_string();
    let derived_param_uuid = Uuid::parse_str(&output_parameter_id).unwrap();

    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"INSERT INTO site_parameters
            (id, site_id, parameter_id, name, sensor_type, display_units, is_active, is_derived, derived_definition_id)
          VALUES (gen_random_uuid(), $1, $2, $3, 'derived', 'mg/L', true, true, $4)",
        [
            site_id.into(),
            derived_param_uuid.into(),
            derived_name.into(),
            Uuid::parse_str(&derived_def_id).unwrap().into(),
        ],
    ))
    .await
    .unwrap();

    let before = count_derived(&db, site_id, derived_param_uuid).await;
    assert_eq!(before, 0, "no derived readings should exist before janitor");

    let filled = river_db::routes::private::parameters::derived::janitor::run_once(&db, None)
        .await
        .unwrap();
    assert!(filled > 0, "janitor should fill at least one gap");

    let after = count_derived(&db, site_id, derived_param_uuid).await;
    assert!(
        after > 0,
        "derived readings should exist after janitor (filled={filled}, count={after})"
    );

    let base: DateTime<Utc> = "2025-01-15T00:00:00Z".parse().unwrap();
    let sample_value = derived_value_at(&db, site_id, derived_param_uuid, base).await;
    assert!(
        sample_value.is_some(),
        "expected derived reading at the seed base time"
    );

    let new_source_time = base + Duration::days(7);
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, calibrated_value, replicate_index, measurement_type)
          VALUES (
            (SELECT id FROM data_streams WHERE site_parameter_id = (
                SELECT id FROM site_parameters WHERE site_id = $1 AND parameter_id = $2
            ) LIMIT 1),
            $1, $2, $3, $4, $4, 0, 'continuous'
          )",
        [
            site_id.into(),
            Uuid::parse_str(crate::common::GLOBAL_PARAM_DO_ID).unwrap().into(),
            new_source_time.into(),
            500.0_f64.into(),
        ],
    ))
    .await
    .unwrap();

    let pre_second = derived_value_at(&db, site_id, derived_param_uuid, new_source_time).await;
    assert!(
        pre_second.is_none(),
        "derived reading should not exist before second janitor run"
    );

    let filled_again = river_db::routes::private::parameters::derived::janitor::run_once(&db, None)
        .await
        .unwrap();
    assert!(filled_again >= 1, "second janitor run should heal new gap");

    let v = derived_value_at(&db, site_id, derived_param_uuid, new_source_time).await;
    assert!(
        v.is_some(),
        "derived reading should be filled by second janitor run"
    );
    let got = v.unwrap();
    let expected = 500.0 * 0.032;
    assert!(
        (got - expected).abs() < 1e-6,
        "expected {expected}, got {got}"
    );
}
