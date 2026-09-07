//! What an instrument's inventory row states depends on what it is.
//!
//! A field device has a deployment, so its row carries the site it is at. A lab instrument never
//! has one, so the Site column is empty for it however long it has been in use; what it has is the
//! curves fitted on it and the newest reading any of them corrected.
//!
//! Run: cargo test --test sensors lab_instrument_row -- --test-threads=1

use serial_test::serial;
use uuid::Uuid;

use crate::common::sensor_lifecycle as sl;

async fn sensor_json(app: &axum::Router, token: &str, id: Uuid) -> serde_json::Value {
    let (status, body) =
        crate::common::get_with_token(app, &format!("/api/sensors/{id}"), token).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
#[serial]
async fn a_lab_instrument_reports_its_curves_where_a_device_reports_its_site() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    // A deployed field device.
    let device = sl::create_sensor(&db, "field probe", crate::common::GLOBAL_PARAM_TEMP_ID).await;
    sl::deploy_sensor(
        &db,
        device.id,
        crate::common::SITE1_ID,
        sl::dt("2025-06-01T00:00:00Z"),
    )
    .await;

    // A lab instrument: two curves, one of which corrected a stored reading.
    let lab = sl::create_sensor_without_curve(&db, "titrator").await;
    crate::common::exec(
        &db,
        &format!("UPDATE sensors SET kind = 'lab', is_lab_instrument = TRUE WHERE id = '{lab}'"),
    )
    .await;
    let used_curve = Uuid::new_v4();
    for (id, name) in [
        (used_curve, "DOC corr 2025"),
        (Uuid::new_v4(), "DOC corr 2024"),
    ] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO standard_curves (id, sensor_id, slope, intercept, name, fitted_on, \
                 created_at) VALUES ('{id}', '{lab}', 2.0, 1.0, '{name}', '2025-01-01', NOW())"
            ),
        )
        .await;
    }
    let stream =
        sl::create_paired_stream(&db, "titrator-feed", crate::common::PARAM_S1_TEMP_ID).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, time, replicate_index, raw_value, sensor_id, \
             standard_curve_id, measurement_type, is_flagged) \
             VALUES ('{stream}', '2025-07-04T09:00:00Z', 0, 5.0, '{lab}', '{used_curve}', \
             'spot', false)"
        ),
    )
    .await;

    let device_row = sensor_json(&app, &token, device.id).await;
    assert_eq!(
        device_row["current_site_id"],
        serde_json::json!(crate::common::SITE1_ID),
        "a device states where it is: {device_row}"
    );
    assert_eq!(
        device_row["curve_count"],
        serde_json::json!(0),
        "and owns no curves: {device_row}"
    );

    let lab_row = sensor_json(&app, &token, lab).await;
    assert!(
        lab_row["current_site_id"].is_null(),
        "a lab instrument has no deployment, so the site column does not apply: {lab_row}"
    );
    assert_eq!(
        lab_row["curve_count"],
        serde_json::json!(2),
        "it states the curves fitted on it instead: {lab_row}"
    );
    assert_eq!(
        lab_row["last_curve_use"]
            .as_str()
            .map(|t| t.starts_with("2025-07-04")),
        Some(true),
        "and when one of them last corrected a reading: {lab_row}"
    );
}

#[tokio::test]
#[serial]
async fn an_instrument_whose_curves_corrected_nothing_reports_a_count_and_no_use() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    sl::seed_base_entities(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let lab = sl::create_sensor_without_curve(&db, "unused titrator").await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept, name, fitted_on, \
             created_at) VALUES (gen_random_uuid(), '{lab}', 1.0, 0.0, 'fresh', '2025-01-01', NOW())"
        ),
    )
    .await;

    let row = sensor_json(&app, &token, lab).await;
    assert_eq!(row["curve_count"], serde_json::json!(1), "{row}");
    assert!(
        row["last_curve_use"].is_null(),
        "a curve nobody has used yet is not a use: {row}"
    );
}
