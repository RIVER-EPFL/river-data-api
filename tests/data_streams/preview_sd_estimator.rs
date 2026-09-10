//! `/streams/{id}/preview` shows the statistics pairing will produce, so its standard deviation
//! must use the divisor the slot declares rather than a divisor of its own.
//!
//! Run with: cargo test --test data_streams preview_sd_estimator

use sea_orm::DatabaseConnection;
use serial_test::serial;
use uuid::Uuid;

const INSTANT: &str = "2025-03-04T09:00:00Z";

async fn seed_stream(db: &DatabaseConnection, paired: bool) -> Uuid {
    let stream_id = Uuid::new_v4();
    let pairing = if paired {
        format!(
            ", site_parameter_id, paired_at) VALUES ('{stream_id}', 'metalp', '{}', 'Portal lab column', true, '{}', NOW()",
            Uuid::new_v4(),
            crate::common::PARAM_S1_TEMP_ID
        )
    } else {
        format!(
            ") VALUES ('{stream_id}', 'metalp', '{}', 'Portal lab column', true",
            Uuid::new_v4()
        )
    };
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active{pairing})"
        ),
    )
    .await;
    for (idx, value) in [(0, 5.0), (1, 6.0), (2, 7.0)] {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO readings (stream_id, time, replicate_index, raw_value, measurement_type) \
                 VALUES ('{stream_id}', '{INSTANT}', {idx}, {value}, 'spot')"
            ),
        )
        .await;
    }
    stream_id
}

#[tokio::test]
#[serial]
async fn preview_uses_the_slot_declared_divisor() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let stream_id = seed_stream(&db, true).await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE site_parameters SET sd_estimator = 'population' WHERE id = '{}'",
            crate::common::PARAM_S1_TEMP_ID
        ),
    )
    .await;

    let (status, json) = crate::common::get_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/preview"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "preview: {json}");

    assert_eq!(json["sd_estimator"], "population");
    assert_eq!(json["sd_estimator_source"], "slot");
    let sd = json["instants"][0]["sd"].as_f64().unwrap();
    // 5, 6, 7 under the n divisor: sqrt(2/3).
    assert!(
        (sd - (2.0f64 / 3.0).sqrt()).abs() < 1e-9,
        "population sd expected, got {sd}"
    );
}

#[tokio::test]
#[serial]
async fn an_undeclared_slot_previews_a_sample_sd_and_says_so() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let stream_id = seed_stream(&db, false).await;

    let (status, json) = crate::common::get_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/preview"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "preview: {json}");

    assert_eq!(json["sd_estimator"], "sample");
    assert_eq!(json["sd_estimator_source"], "default");
    let sd = json["instants"][0]["sd"].as_f64().unwrap();
    assert!((sd - 1.0).abs() < 1e-9, "sample sd expected, got {sd}");
}
