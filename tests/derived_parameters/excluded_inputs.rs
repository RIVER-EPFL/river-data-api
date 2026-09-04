//! Scenario: a source reading at an instant is retracted (withdrawn) or flagged, and a derived
//! parameter reads that parameter as a formula variable.
//!
//! Expected behaviour: the instant resolves no input, so no derived reading is written there,
//! while the neighbouring unflagged instants still compute.
//!
//! Run with: cargo test --test derived_parameters excluded_inputs

use chrono::{DateTime, Duration, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

const POLL_DEADLINE_SECS: u64 = 30;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

async fn derived_value_at(
    db: &DatabaseConnection,
    parameter_id: Uuid,
    time: DateTime<Utc>,
) -> Option<f64> {
    db.query_one_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT COALESCE(calibrated_value, raw_value) AS value FROM readings \
         WHERE site_id = $1 AND parameter_id = $2 AND time = $3 LIMIT 1",
        [
            Uuid::parse_str(crate::common::SITE1_ID).unwrap().into(),
            parameter_id.into(),
            time.into(),
        ],
    ))
    .await
    .ok()
    .flatten()
    .and_then(|r| r.try_get::<f64>("", "value").ok())
}

async fn poll_for_derived(
    db: &DatabaseConnection,
    parameter_id: Uuid,
    time: DateTime<Utc>,
    max_seconds: u64,
) -> Option<f64> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(max_seconds);
    while std::time::Instant::now() < deadline {
        if let Some(v) = derived_value_at(db, parameter_id, time).await {
            return Some(v);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    None
}

/// Define a derived parameter over Dissolved_O2 at SITE1, assign it, run the recompute action,
/// and return its output parameter id.
async fn define_assign_recompute(app: &axum::Router, token: &str, label: &str) -> Uuid {
    let code = format!("{label}_{}", Uuid::new_v4().simple());
    let (status, def_json) = crate::common::post_json_parse_with_token(
        app,
        "/api/derived_parameters",
        &serde_json::json!({
            "code": code,
            "name": "Excluded input fixture",
            "units": "mg/L",
            "formula": "Dissolved_O2 * 0.032",
        }),
        token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create derived ({status}): {def_json}"
    );
    let def_id = def_json["id"].as_str().expect("def id").to_string();
    let output_parameter_id = def_json["output_parameter_id"]
        .as_str()
        .expect("output parameter id")
        .to_string();

    let (status, text) = crate::common::post_json_with_token(
        app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "parameter_id": output_parameter_id,
            "name": code,
            "sensor_type": "derived",
            "is_derived": true,
            "derived_definition_id": def_id,
            "display_units": "mg/L",
        }),
        token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "assign site_parameter ({status}): {text}"
    );

    let uri = format!("/api/actions/derived_parameters/{def_id}/recompute");
    let (status, text) =
        crate::common::post_json_with_token(app, &uri, &serde_json::json!({}), token).await;
    assert!((200..300).contains(&status), "recompute ({status}): {text}");

    Uuid::parse_str(&output_parameter_id).unwrap()
}

#[tokio::test]
#[serial]
async fn flagged_input_produces_no_derived_reading() {
    let (db, app, token) = setup().await;
    let flagged_time = crate::common::base_time();
    let neighbour_time = flagged_time + Duration::minutes(10);

    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET is_flagged = true \
             WHERE site_id = '{site}' AND parameter_id = '{param}' AND time = '{time}'",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_DO_ID,
            time = flagged_time.to_rfc3339(),
        ),
    )
    .await;

    let derived_param = define_assign_recompute(&app, &token, "dom_flagged").await;

    assert!(
        poll_for_derived(&db, derived_param, neighbour_time, POLL_DEADLINE_SECS)
            .await
            .is_some(),
        "recompute did not run: the unflagged neighbouring instant has no derived reading"
    );
    assert_eq!(
        derived_value_at(&db, derived_param, flagged_time).await,
        None,
        "a flagged input must not produce a derived reading at {flagged_time}"
    );
}

#[tokio::test]
#[serial]
async fn withdrawn_input_produces_no_derived_reading() {
    let (db, app, token) = setup().await;
    // Off the seeded 10-minute grid so the withdrawn spot row is the only reading at its instant.
    let withdrawn_time: DateTime<Utc> = "2025-01-15T00:05:30Z".parse().unwrap();
    let neighbour_time = crate::common::base_time() + Duration::minutes(10);

    let stream_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active, measurement_type) \
             VALUES ('{stream_id}', 'grab_sample', '{}', true, 'spot')",
            Uuid::new_v4()
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                 raw_value, measurement_type, withdrawn_at) \
             VALUES ('{stream_id}', '{site}', '{param}', '{time}', 0, 250.0, 'spot', NOW())",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_DO_ID,
            time = withdrawn_time.to_rfc3339(),
        ),
    )
    .await;

    let derived_param = define_assign_recompute(&app, &token, "dom_withdrawn").await;

    assert!(
        poll_for_derived(&db, derived_param, neighbour_time, POLL_DEADLINE_SECS)
            .await
            .is_some(),
        "recompute did not run: a live instant has no derived reading"
    );
    assert_eq!(
        derived_value_at(&db, derived_param, withdrawn_time).await,
        None,
        "a withdrawn input must not produce a derived reading at {withdrawn_time}"
    );
}
