//! Scenario: two streams paired to one slot both report a continuous reading at one instant.
//! Expected behaviour: the site chart draws the instant once, at the mean of the readings the
//! instant serves, with the severity of that mean. A flagged reading at the instant is not pooled.
//!
//! Run: cargo test --test sites two_stream_instants -- --test-threads=1

use sea_orm::DatabaseConnection;
use serial_test::serial;
use uuid::Uuid;

const INSTANT: &str = "2025-02-01T10:00:00Z";

async fn stream_reporting(db: &DatabaseConnection, value: f64, flagged: bool) {
    let stream_id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active, site_parameter_id) \
             VALUES ('{stream_id}', 'twostream', '{}', true, '{}')",
            Uuid::new_v4(),
            crate::common::PARAM_S1_TEMP_ID
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                raw_value, measurement_type, is_flagged) \
             VALUES ('{stream_id}', '{}', '{}', '{INSTANT}', 0, {value}, 'continuous', {flagged})",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
}

#[tokio::test]
#[serial]
async fn a_continuous_instant_two_streams_feed_is_drawn_once_at_their_mean() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    // Either reading alone breaches one bound; their mean breaches neither.
    let (param, site) = (crate::common::GLOBAL_PARAM_TEMP_ID, crate::common::SITE1_ID);
    crate::common::exec(
        &db,
        &format!(
            "DELETE FROM alarm_thresholds WHERE parameter_id = '{param}' AND site_id = '{site}'"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO alarm_thresholds (parameter_id, site_id, alarm_min, alarm_max) \
             VALUES ('{param}', '{site}', 11.0, 13.0)"
        ),
    )
    .await;
    stream_reporting(&db, 10.0, false).await;
    stream_reporting(&db, 14.0, false).await;
    stream_reporting(&db, 100.0, true).await;

    let url = format!(
        "/api/sites/{}/readings?start=2025-02-01T09:00:00Z&end=2025-02-01T11:00:00Z\
         &parameter_ids={}&alarms=true",
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID
    );
    let (status, body) = crate::common::get_json_with_token(&app, &url, &token).await;
    assert!((200..300).contains(&status), "readings ({status}): {body}");
    assert_eq!(
        body["times"].as_array().map(Vec::len),
        Some(1),
        "one point: {body}"
    );
    let param = &body["parameters"][0];
    let value = param["values"][0].as_f64().expect("a value");
    // (10.0 + 14.0) / 2: the flagged 100.0 is not pooled
    assert!((value - 12.0).abs() < 1e-9, "the served value: {body}");
    assert_eq!(param["flagged"][0], false, "{body}");
    assert_eq!(
        param["severities"][0], 0,
        "the severity of the mean: {body}"
    );
}
