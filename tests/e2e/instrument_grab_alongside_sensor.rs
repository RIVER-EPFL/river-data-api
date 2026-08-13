//! A lab instrument's grab samples coexist with a continuous field sensor at the same
//! site+parameter: the two arrive through different paths (grab entry with an instant curve vs
//! batch ingestion under a deployment), stay separable by `measurement_type`, keep their own curve
//! provenance, and continuous aggregates roll up only the sensor stream.
//!
//! Run: cargo test --test e2e -- --test-threads=1

use crate::common::e2e;
use crate::common::sensor_lifecycle as sl;
use sea_orm::{ConnectionTrait, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

const RANGE: &str = "start=2025-06-01T00:00:00Z&end=2025-06-01T01:00:00Z";

#[tokio::test]
#[serial]
async fn instrument_grabs_coexist_with_continuous_sensor_stream() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let site = crate::common::SITE1_ID;
    let param = crate::common::GLOBAL_PARAM_TEMP_ID;

    // Continuous side: a deployed field sensor whose stream batch-ingests six readings.
    let field_sensor = sl::create_sensor(&db, "field-probe", param).await;
    let deployment =
        sl::deploy_sensor(&db, field_sensor.id, site, sl::dt("2025-05-01T00:00:00Z")).await;

    let continuous: Vec<serde_json::Value> = (0..6)
        .map(|i| {
            json!({
                "site_id": site, "parameter_id": param,
                "time": format!("2025-06-01T00:{:02}:00Z", i * 10),
                "raw_value": 10.0 + i as f64,
            })
        })
        .collect();
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &json!({ "readings": continuous }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "batch insert ({status}): {body}"
    );

    // Lab side: an instrument with an instant curve submits a grab in the middle of the window.
    let instrument_id = "00000000-0000-4000-c000-00000000c0e1";
    let curve_id = "00000000-0000-4000-c000-00000000c0e2";
    for sql in [
        format!(
            "INSERT INTO sensors (id, name, is_active, is_lab_instrument, created_at) \
             VALUES ('{instrument_id}', 'Microplate reader', true, true, now())"
        ),
        format!(
            "INSERT INTO sensor_calibrations (id, sensor_id, slope, intercept, valid_from, mode, name) \
             VALUES ('{curve_id}', '{instrument_id}', 2.0, 1.0, now(), 'instant', 'Plate A')"
        ),
    ] {
        db.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            sql,
        ))
        .await
        .unwrap();
    }

    let grab_time = "2025-06-01T00:35:00Z";
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": site, "created_by": "e2e",
            "readings": [
                { "parameter_id": param, "sensor_id": instrument_id,
                  "calibration_id": curve_id, "value": 100.0, "time": grab_time }
            ],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab with instant curve ({status}): {body}");

    // measurement_type separates the two populations, disjointly.
    let (status, cont) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site}/readings?{RANGE}&measurement_type=continuous"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{cont}");
    let cont_vals = e2e::values_for(&cont, param);
    assert_eq!(
        cont_vals.len(),
        6,
        "continuous view holds only the sensor stream: {cont}"
    );
    assert!(
        cont_vals.iter().all(|v| (10.0..=15.0).contains(v)),
        "continuous values untouched by the grab: {cont_vals:?}"
    );

    let (status, spot) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site}/readings?{RANGE}&measurement_type=spot"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{spot}");
    let spot_vals = e2e::values_for(&spot, param);
    assert_eq!(spot_vals.len(), 1, "spot view holds only the grab: {spot}");
    assert!(
        (spot_vals[0] - 201.0).abs() < 1e-6,
        "grab served curve-corrected (2.0 * 100 + 1.0): {spot_vals:?}"
    );

    // Per-row provenance: the grab carries the instant curve and no deployment; the sensor
    // stream keeps its deployment attribution.
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sensor_id, calibration_id, deployment_id, measurement_type FROM readings \
             WHERE site_id = $1::uuid AND parameter_id = $2::uuid AND time = $3",
            [site.into(), param.into(), sl::dt(grab_time).into()],
        ))
        .await
        .unwrap()
        .expect("grab reading exists");
    let grab_sensor: Option<Uuid> = row.try_get("", "sensor_id").ok();
    let grab_curve: Option<Uuid> = row.try_get("", "calibration_id").ok();
    let grab_dep: Option<Uuid> = row.try_get("", "deployment_id").ok();
    let grab_mtype: Option<String> = row.try_get("", "measurement_type").ok();
    assert_eq!(
        grab_sensor.map(|u| u.to_string()),
        Some(instrument_id.to_string())
    );
    assert_eq!(
        grab_curve.map(|u| u.to_string()),
        Some(curve_id.to_string())
    );
    assert_eq!(grab_dep, None, "grabs create no deployment");
    assert_eq!(grab_mtype.as_deref(), Some("spot"));

    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sensor_id, deployment_id FROM readings \
             WHERE site_id = $1::uuid AND parameter_id = $2::uuid AND time = $3",
            [
                site.into(),
                param.into(),
                sl::dt("2025-06-01T00:30:00Z").into(),
            ],
        ))
        .await
        .unwrap()
        .expect("continuous reading exists");
    let cont_sensor: Option<Uuid> = row.try_get("", "sensor_id").ok();
    let cont_dep: Option<Uuid> = row.try_get("", "deployment_id").ok();
    assert_eq!(
        cont_sensor,
        Some(field_sensor.id),
        "sensor attribution undisturbed by the grab"
    );
    assert_eq!(cont_dep, Some(deployment));

    // Continuous aggregates roll up only the sensor stream: the grab's 201 would drag the
    // hourly mean far off 12.5 if it leaked in.
    let (status, refresh) = crate::common::post_json_with_token(
        &app,
        "/api/actions/refresh_aggregates",
        &json!({ "full": true }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "refresh_aggregates ({status}): {refresh}"
    );
    let refresh: serde_json::Value = serde_json::from_str(&refresh).unwrap();
    let job_id = refresh["job_id"].as_str().expect("tracked refresh job");
    let final_status = e2e::poll_job(&app, &token, job_id, 30).await;
    assert_eq!(final_status, "completed");

    let (status, agg) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site}/aggregates/hourly?{RANGE}"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{agg}");
    let avg = e2e::field_for(&agg, param, "avg");
    assert!(
        avg.first().is_some_and(|v| (v - 12.5).abs() < 1e-6),
        "hourly avg is the continuous mean (10..15 -> 12.5), grab excluded: {avg:?}"
    );

    // include_measurement_type labels every point so clients can split the mixed view.
    let (status, mixed) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site}/readings?{RANGE}&include_measurement_type=true"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{mixed}");
    let series = mixed["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["parameter_id"] == param)
        .unwrap_or_else(|| panic!("parameter missing in {mixed}"));
    let mtypes: Vec<Option<&str>> = series["measurement_types"]
        .as_array()
        .unwrap_or_else(|| panic!("measurement_types missing: {mixed}"))
        .iter()
        .map(|v| v.as_str())
        .collect();
    assert_eq!(mtypes.len(), 7, "every point labelled: {mixed}");
    assert_eq!(
        mtypes.iter().filter(|m| **m == Some("spot")).count(),
        1,
        "one grab point: {mtypes:?}"
    );
    assert_eq!(
        mtypes.iter().filter(|m| **m != Some("spot")).count(),
        6,
        "six sensor-stream points: {mtypes:?}"
    );
}
