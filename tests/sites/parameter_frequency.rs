//! The frequency classification on `GET /sites/{id}/parameters`: `has_continuous`/`has_spot`
//! counts ride the extent scan and `frequency` derives from them ('low' = spot-only,
//! 'mixed' = both, 'high' otherwise). The UI uses these to default charts to marker-only
//! rendering for low-frequency (lab/campaign) series.
//!
//! Run: cargo test --test sites -- --test-threads=1

use serial_test::serial;
use uuid::Uuid;

#[tokio::test]
#[serial]
async fn parameters_report_frequency_from_measurement_types() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let site_id = crate::common::SITE1_ID;

    // Turn one seeded parameter into a mixed series (continuous seed data + one spot grab), and
    // one into spot-only by retagging all its readings.
    let stream_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active) \
             VALUES ('{stream_id}', 'grab_sample', '{}', true)",
            Uuid::new_v4()
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                raw_value, calibrated_value, measurement_type) \
             VALUES ('{stream_id}', '{site_id}', '{param}', '2025-01-15T06:00:00Z', 1, 7.0, 7.0, 'spot')",
            param = crate::common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET measurement_type = 'spot' \
             WHERE site_id = '{site_id}' AND parameter_id = '{param}'",
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    )
    .await;

    // The extents read the maintained summaries, not the hypertable: continuous presence from
    // the hourly aggregate, spot presence from `samples`. Re-refresh so the retag leaves the
    // rollup, and materialise the spot groups as the write paths would have.
    crate::common::refresh_continuous_aggregates(&db).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO samples (site_id, parameter_id, collected_at, n) \
             VALUES ('{site_id}', '{param}', '2025-01-15T06:00:00Z', 1)",
            param = crate::common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO samples (site_id, parameter_id, collected_at, n) \
             SELECT site_id, parameter_id, MIN(time), 2 FROM readings \
             WHERE site_id = '{site_id}' AND parameter_id = '{param}' \
             GROUP BY site_id, parameter_id",
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    )
    .await;

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site_id}/parameters"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "parameters ({status}): {body}");

    let params = body.as_array().expect("array body");
    let by_param = |id: &str| {
        params
            .iter()
            .find(|p| p["parameter_id"] == id)
            .unwrap_or_else(|| panic!("parameter {id} missing from response"))
    };

    let mixed = by_param(crate::common::GLOBAL_PARAM_TEMP_ID);
    assert_eq!(mixed["frequency"], "mixed", "temp: {mixed}");
    assert_eq!(mixed["has_spot"], true);
    assert_eq!(mixed["has_continuous"], true);

    let low = by_param(crate::common::GLOBAL_PARAM_TURB_ID);
    assert_eq!(low["frequency"], "low", "turbidity: {low}");
    assert_eq!(low["has_spot"], true);
    assert_eq!(low["has_continuous"], false);

    let high = by_param(crate::common::GLOBAL_PARAM_DO_ID);
    assert_eq!(high["frequency"], "high", "do: {high}");
    assert_eq!(high["has_spot"], false);
    assert_eq!(high["has_continuous"], true);
}
