//! `GET /sites/{id}/parameters` extents assembled from the maintained summaries (hourly
//! aggregate, samples, stream cursors, bounded recent pass) instead of an unbounded hypertable
//! scan. Seeded readings run 2025-01-15T00:00Z..2025-01-16T23:50Z at 10-minute steps (288 rows
//! per parameter).
//!
//! Run: cargo test --test sites parameter_extents -- --test-threads=1

use serial_test::serial;

async fn temp_extent(
    app: &axum::Router,
    token: &str,
) -> serde_json::Value {
    let (status, body) = crate::common::get_json_with_token(
        app,
        &format!("/api/sites/{}/parameters", crate::common::SITE1_ID),
        token,
    )
    .await;
    assert_eq!(status, 200, "parameters ({status}): {body}");
    body.as_array()
        .expect("array body")
        .iter()
        .find(|p| p["parameter_id"] == crate::common::GLOBAL_PARAM_TEMP_ID)
        .expect("temp parameter present")
        .clone()
}

#[tokio::test]
#[serial]
async fn extents_come_from_summaries() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let temp = temp_extent(&app, &token).await;
    assert_eq!(temp["data_start"], "2025-01-15T00:00:00Z", "{temp}");
    // The aggregate reports the last bucket start; the extent covers that whole bucket.
    assert_eq!(temp["data_end"], "2025-01-17T00:00:00Z", "{temp}");
    assert_eq!(temp["reading_count"], 288, "{temp}");
    assert_eq!(temp["has_continuous"], true);
    assert_eq!(temp["has_spot"], false);
}

#[tokio::test]
#[serial]
async fn flagged_tail_and_fresh_rows_extend_extents() {
    // Scenario: the newest day of a series is flagged (leaving the rollup on refresh), while its
    // stream cursor still records the true newest instant; later a reading arrives too new for
    // any refresh.
    // Expected behaviour: data_end never retreats behind the cursor, the fresh row extends it,
    // and the count follows the rollup's unflagged population.
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let site_id = crate::common::SITE1_ID;
    let param = crate::common::GLOBAL_PARAM_TEMP_ID;

    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET is_flagged = TRUE \
             WHERE site_id = '{site_id}' AND parameter_id = '{param}' AND time >= '2025-01-16T00:00:00Z'"
        ),
    )
    .await;
    crate::common::refresh_continuous_aggregates(&db).await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE data_streams SET last_data_time = '2025-01-16T23:50:00Z' \
             WHERE site_parameter_id = '{}'",
            crate::common::PARAM_S1_TEMP_ID
        ),
    )
    .await;

    let temp = temp_extent(&app, &token).await;
    assert_eq!(
        temp["data_end"], "2025-01-16T23:50:00Z",
        "the cursor keeps the flagged tail inside the extent: {temp}"
    );
    assert_eq!(temp["reading_count"], 144, "unflagged rollup population: {temp}");
    assert_eq!(temp["has_continuous"], true);

    let fresh = (chrono::Utc::now() - chrono::Duration::hours(1))
        .with_time(chrono::NaiveTime::from_hms_opt(6, 0, 0).unwrap())
        .unwrap();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value) \
             SELECT id, '{site_id}', '{param}', '{}', 1.0 FROM data_streams \
             WHERE site_parameter_id = '{}'",
            fresh.to_rfc3339(),
            crate::common::PARAM_S1_TEMP_ID
        ),
    )
    .await;

    let temp = temp_extent(&app, &token).await;
    let data_end: chrono::DateTime<chrono::Utc> = temp["data_end"]
        .as_str()
        .expect("data_end present")
        .parse()
        .unwrap();
    assert_eq!(
        data_end, fresh,
        "a reading too new for any refresh extends the extent through the bounded pass: {temp}"
    );
}
