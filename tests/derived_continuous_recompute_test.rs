//! Scenario: a derived parameter is defined, assigned to a site, then source
//! readings arrive via the ingest path.
//!
//! Expected behaviour: each source reading triggers a derived reading at the
//! same timestamp with `value = formula(source_value)`, without any manual
//! recompute action.
//!
//! Run with: cargo test --test derived_continuous_recompute_test

mod common;

use chrono::{DateTime, Duration, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

const POLL_DEADLINE_SECS: u64 = 30;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());
    (db, app, token)
}

async fn poll_for_derived(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
    time: DateTime<Utc>,
    max_seconds: u64,
) -> Option<f64> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(max_seconds);
    while std::time::Instant::now() < deadline {
        let row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT calibrated_value FROM readings \
                 WHERE site_id = $1 AND parameter_id = $2 AND time = $3 \
                 LIMIT 1",
                [site_id.into(), parameter_id.into(), time.into()],
            ))
            .await
            .ok()
            .flatten();
        if let Some(r) = row
            && let Ok(v) = r.try_get::<f64>("", "calibrated_value")
        {
            return Some(v);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    None
}

#[tokio::test]
#[serial]
async fn test_continuous_derived_recompute_after_ingest() {
    let (db, app, token) = setup().await;
    let site_id = Uuid::parse_str(common::SITE1_ID).unwrap();

    let derived_name = format!("dom_test_{}", Uuid::new_v4().simple());
    let create_body = serde_json::json!({
        "name": derived_name,
        "display_name": "Test DO mg/L",
        "units": "mg/L",
        "formula": "Dissolved_O2 * 0.032",
        "description": "Continuous recompute test fixture",
    });
    let (status, def_json) = common::post_json_parse_with_token(
        &app,
        "/api/v1/derived_parameters",
        &create_body,
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create derived ({status}): {def_json}"
    );
    let derived_def_id = def_json["id"].as_str().expect("def id").to_string();
    let output_parameter_id = def_json["output_parameter_id"]
        .as_str()
        .expect("derived output parameter_id must be populated by after_create hook")
        .to_string();
    let derived_param_uuid = Uuid::parse_str(&output_parameter_id).unwrap();

    let assign_body = serde_json::json!({
        "site_id": common::SITE1_ID,
        "parameter_id": output_parameter_id,
        "name": derived_name,
        "sensor_type": "derived",
        "is_derived": true,
        "derived_definition_id": derived_def_id,
        "display_units": "mg/L",
    });
    let (status, sp_text) = common::post_json_with_token(
        &app,
        "/api/v1/site_parameters",
        &assign_body,
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "assign site_parameter ({status}): {sp_text}"
    );

    let t1: DateTime<Utc> = Utc::now() - Duration::hours(36);
    let t1 = t1.with_timezone(&Utc) - Duration::nanoseconds(i64::from(t1.timestamp_subsec_nanos()));
    let raw_value_1 = 250.0_f64;
    let ingest_body_1 = serde_json::json!({
        "readings": [{
            "site_id": common::SITE1_ID,
            "parameter_id": common::GLOBAL_PARAM_DO_ID,
            "time": t1.to_rfc3339(),
            "raw_value": raw_value_1,
        }]
    });
    let (status, text) = common::post_json_with_token(
        &app,
        "/api/v1/readings/batch",
        &ingest_body_1,
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "ingest source #1 ({status}): {text}"
    );

    let expected_1 = raw_value_1 * 0.032;
    let v1 = poll_for_derived(&db, site_id, derived_param_uuid, t1, POLL_DEADLINE_SECS).await;
    assert!(
        v1.is_some(),
        "derived reading at {t1} did not appear within {POLL_DEADLINE_SECS}s of ingest"
    );
    let got_1 = v1.unwrap();
    assert!(
        (got_1 - expected_1).abs() < 1e-6,
        "first derived value: expected {expected_1}, got {got_1}"
    );

    let t2 = t1 + Duration::minutes(10);
    let raw_value_2 = 300.0_f64;
    let ingest_body_2 = serde_json::json!({
        "readings": [{
            "site_id": common::SITE1_ID,
            "parameter_id": common::GLOBAL_PARAM_DO_ID,
            "time": t2.to_rfc3339(),
            "raw_value": raw_value_2,
        }]
    });
    let (status, text) = common::post_json_with_token(
        &app,
        "/api/v1/readings/batch",
        &ingest_body_2,
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "ingest source #2 ({status}): {text}"
    );

    let expected_2 = raw_value_2 * 0.032;
    let v2 = poll_for_derived(&db, site_id, derived_param_uuid, t2, POLL_DEADLINE_SECS).await;
    assert!(
        v2.is_some(),
        "second derived reading at {t2} did not appear within {POLL_DEADLINE_SECS}s of ingest"
    );
    let got_2 = v2.unwrap();
    assert!(
        (got_2 - expected_2).abs() < 1e-6,
        "second derived value: expected {expected_2}, got {got_2}"
    );

    let v1_again = poll_for_derived(&db, site_id, derived_param_uuid, t1, 2).await;
    assert!(
        v1_again.is_some(),
        "first derived reading was lost after second ingest"
    );
}

/// Scenario: a source reading exists historically but no derived reading was
/// computed for that timestamp. Triggering the manual recompute endpoint must
/// backfill the missing derived value.
#[tokio::test]
#[serial]
async fn test_recompute_endpoint_backfills_historical_gap() {
    let (db, app, token) = setup().await;
    let site_id = Uuid::parse_str(common::SITE1_ID).unwrap();

    let derived_name = format!("dom_backfill_{}", Uuid::new_v4().simple());
    let create_body = serde_json::json!({
        "name": derived_name,
        "display_name": "Backfill DO mg/L",
        "units": "mg/L",
        "formula": "Dissolved_O2 * 0.032",
    });
    let (status, def_json) = common::post_json_parse_with_token(
        &app,
        "/api/v1/derived_parameters",
        &create_body,
        &token,
    )
    .await;
    assert!((200..300).contains(&status));
    let derived_def_id = def_json["id"].as_str().unwrap().to_string();
    let output_parameter_id = def_json["output_parameter_id"].as_str().unwrap().to_string();
    let derived_param_uuid = Uuid::parse_str(&output_parameter_id).unwrap();

    let seeded_source_time: DateTime<Utc> = "2025-01-15T00:00:00Z".parse().unwrap();
    let pre = poll_for_derived(&db, site_id, derived_param_uuid, seeded_source_time, 1).await;
    assert!(
        pre.is_none(),
        "expected no derived reading before assignment + recompute"
    );

    let assign_body = serde_json::json!({
        "site_id": common::SITE1_ID,
        "parameter_id": output_parameter_id,
        "name": derived_name,
        "sensor_type": "derived",
        "is_derived": true,
        "derived_definition_id": derived_def_id,
        "display_units": "mg/L",
    });
    let (status, _) = common::post_json_with_token(
        &app,
        "/api/v1/site_parameters",
        &assign_body,
        &token,
    )
    .await;
    assert!((200..300).contains(&status));

    let uri = format!("/api/v1/actions/derived_parameters/{derived_def_id}/recompute");
    let (status, _) = common::post_json_with_token(&app, &uri, &serde_json::json!({}), &token).await;
    assert!(
        (200..300).contains(&status),
        "recompute endpoint should accept request"
    );

    let v = poll_for_derived(&db, site_id, derived_param_uuid, seeded_source_time, POLL_DEADLINE_SECS).await;
    assert!(
        v.is_some(),
        "recompute should have backfilled a derived reading at {seeded_source_time}"
    );
}
