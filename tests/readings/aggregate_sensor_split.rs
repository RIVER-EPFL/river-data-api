//! `split_by_sensor=true` returns one aggregate series per sensor (using the sensor-dimension
//! continuous aggregate), while the default collapses the sensor dimension into a single series.
//!
//! Run: cargo test --test readings -- --test-threads=1


use crate::common::sensor_lifecycle as sl;
use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;

async fn refresh_hourly(db: &sea_orm::DatabaseConnection) {
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "CALL refresh_continuous_aggregate('readings_hourly', '2025-01-14', '2025-01-16')".to_owned(),
    ))
    .await
    .expect("refresh hourly CAGG");
}

#[tokio::test]
#[serial]
async fn aggregates_split_by_sensor_returns_one_series_per_sensor() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let site1 = crate::common::SITE1_ID;
    let temp = crate::common::GLOBAL_PARAM_TEMP_ID;

    // Two sensors hold the (site 1, Temperature) slot in succession.
    let sensor_a = sl::create_sensor(&db, "agg-a", temp).await;
    let cal_a = sl::add_calibration(&db, sensor_a.id, 1.0, 0.0, sl::dt("2025-01-15T00:00:00Z")).await;
    let dep_a = sl::deploy_sensor(&db, sensor_a.id, site1, sl::dt("2025-01-15T00:00:00Z")).await;
    sl::end_deployment(&db, dep_a, sl::dt("2025-01-15T02:00:00Z")).await;
    let sensor_b = sl::create_sensor(&db, "agg-b", temp).await;
    let cal_b = sl::add_calibration(&db, sensor_b.id, 1.0, 0.0, sl::dt("2025-01-15T02:00:00Z")).await;
    let dep_b = sl::deploy_sensor(&db, sensor_b.id, site1, sl::dt("2025-01-15T02:00:00Z")).await;

    let stream = sl::create_paired_stream(&db, "agg-split", crate::common::PARAM_S1_TEMP_ID).await;
    sl::insert_readings(
        &db, stream, site1, temp, sensor_a.id, cal_a, dep_a, 1.0, 0.0,
        &[(sl::dt("2025-01-15T00:15:00Z"), 10.0), (sl::dt("2025-01-15T00:45:00Z"), 12.0)],
    )
    .await;
    sl::insert_readings(
        &db, stream, site1, temp, sensor_b.id, cal_b, dep_b, 1.0, 0.0,
        &[(sl::dt("2025-01-15T02:15:00Z"), 20.0), (sl::dt("2025-01-15T02:45:00Z"), 22.0)],
    )
    .await;
    refresh_hourly(&db).await;

    let base = format!(
        "/api/sites/{site1}/aggregates/hourly?start=2025-01-15T00:00:00Z&end=2025-01-15T06:00:00Z"
    );

    let (status, agg) = crate::common::get_json_with_token(&app, &base, &token).await;
    assert_eq!(status, 200, "collapsed aggregate: {agg}");
    let collapsed: Vec<_> = agg["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["parameter_id"].as_str() == Some(temp))
        .collect();
    assert_eq!(collapsed.len(), 1, "collapsed returns a single Temperature series: {agg}");
    assert!(
        collapsed[0].get("sensor_id").is_none_or(serde_json::Value::is_null),
        "collapsed series omits sensor_id"
    );

    let (status, agg) =
        crate::common::get_json_with_token(&app, &format!("{base}&split_by_sensor=true"), &token).await;
    assert_eq!(status, 200, "split aggregate: {agg}");
    let mut sensor_ids: Vec<String> = agg["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["parameter_id"].as_str() == Some(temp))
        .filter_map(|p| p["sensor_id"].as_str().map(str::to_string))
        .collect();
    sensor_ids.sort();
    let mut expected = vec![sensor_a.id.to_string(), sensor_b.id.to_string()];
    expected.sort();
    assert_eq!(sensor_ids, expected, "split returns one series per sensor with sensor_id: {agg}");
}
