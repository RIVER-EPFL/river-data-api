//! The public tier serves one row per instant, taken at the lowest unflagged replicate index.
//! Which replicate an operator flagged therefore never decides whether the instant is served, and
//! a continuous series, whose instants hold a single row, is served exactly as stored.
//!
//! Run: cargo test --test public_api served_instant_selection -- --test-threads=1

use sea_orm::DatabaseConnection;
use serial_test::serial;
use uuid::Uuid;

const SPOT_TIME: &str = "2025-01-15T00:05:30Z";
const CONTINUOUS_TIME: &str = "2025-01-15T00:07:30Z";
const CONTINUOUS_VALUE: f64 = 13.75;
const WINDOW: &str = "start=2025-01-15T00:00:00Z&end=2025-01-15T01:00:00Z";
const READINGS_URI: &str = "/api/public/test-river/sites/upstream/readings";

/// A public project with one exposed parameter, a two-replicate spot group at one instant and a
/// single continuous reading at another. The spot group carries no sample, so the served value is
/// the picked row's own value rather than a group mean.
async fn setup() -> (DatabaseConnection, axum::Router, Uuid) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    crate::common::exec(
        &db,
        &format!(
            "UPDATE projects SET is_public = true, public_code = 'test-river' WHERE id = '{}'",
            crate::common::PROJECT_ID
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE sites SET public_code = 'upstream' WHERE id = '{}'",
            crate::common::SITE1_ID
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE site_parameters SET is_public = true WHERE id = '{}'",
            crate::common::PARAM_S1_TEMP_ID
        ),
    )
    .await;

    let stream_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active) \
             VALUES ('{stream_id}', 'servedsrc', '{}', true)",
            Uuid::new_v4()
        ),
    )
    .await;
    for (time, idx, value, kind) in [
        (SPOT_TIME, 0, 10.0, "spot"),
        (SPOT_TIME, 1, 20.0, "spot"),
        (CONTINUOUS_TIME, 0, CONTINUOUS_VALUE, "continuous"),
    ] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                    raw_value, measurement_type) \
                 VALUES ('{stream_id}', '{site}', '{param}', '{time}', {idx}, {value}, '{kind}')",
                site = crate::common::SITE1_ID,
                param = crate::common::GLOBAL_PARAM_TEMP_ID,
            ),
        )
        .await;
    }

    let app = crate::common::build_test_app(db.clone());
    (db, app, stream_id)
}

async fn set_flag(db: &DatabaseConnection, stream: Uuid, index: i16, flagged: bool) {
    crate::common::exec(
        db,
        &format!(
            "UPDATE readings SET is_flagged = {flagged} \
             WHERE stream_id = '{stream}' AND time = '{SPOT_TIME}' AND replicate_index = {index}"
        ),
    )
    .await;
}

/// The single value the public endpoint serves for the spot instant, or None when the instant is
/// absent from the response.
async fn served_spot_value(app: &axum::Router) -> Option<f64> {
    let (status, body) = crate::common::get_json(
        app,
        &format!("{READINGS_URI}?{WINDOW}&measurement_type=spot"),
    )
    .await;
    assert_eq!(status, 200, "spot readings ({status}): {body}");
    let times = body["times"].as_array().unwrap();
    let values = body["parameters"][0]["values"].as_array()?;
    assert_eq!(times.len(), values.len(), "{body}");
    assert!(times.len() <= 1, "one spot instant was seeded: {body}");
    values.first().and_then(serde_json::Value::as_f64)
}

#[tokio::test]
#[serial]
async fn flagging_index_zero_keeps_the_instant_and_serves_index_one() {
    let (db, app, stream) = setup().await;
    assert_eq!(served_spot_value(&app).await, Some(10.0));

    set_flag(&db, stream, 0, true).await;
    let value = served_spot_value(&app)
        .await
        .expect("the instant is still served");
    assert!(
        (value - 20.0).abs() < 1e-9,
        "the lowest unflagged index serves: {value}"
    );

    set_flag(&db, stream, 0, false).await;
    set_flag(&db, stream, 1, true).await;
    let value = served_spot_value(&app)
        .await
        .expect("the instant is still served");
    assert!(
        (value - 10.0).abs() < 1e-9,
        "flagging a higher index leaves the instant on its index-0 value: {value}"
    );
}

#[tokio::test]
#[serial]
async fn a_continuous_reading_is_served_as_stored() {
    let (db, app, _stream) = setup().await;

    let (status, body) = crate::common::get_json(
        &app,
        &format!("{READINGS_URI}?{WINDOW}&measurement_type=continuous"),
    )
    .await;
    assert_eq!(status, 200, "continuous readings ({status}): {body}");

    let times = body["times"].as_array().unwrap();
    let values = body["parameters"][0]["values"].as_array().unwrap();
    let seeded = crate::common::e2e::count(
        &db,
        &format!(
            "SELECT COUNT(*) FROM readings WHERE site_id = '{}' AND parameter_id = '{}' \
             AND time >= '2025-01-15T00:00:00Z' AND time <= '2025-01-15T01:00:00Z' \
             AND measurement_type IS DISTINCT FROM 'spot'",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    assert_eq!(
        times.len() as i64,
        seeded,
        "every continuous row in the window is one served point: {body}"
    );

    let position = times
        .iter()
        .position(|t| t.as_str() == Some("2025-01-15 00:07:30"))
        .unwrap_or_else(|| panic!("the off-grid continuous reading is served: {body}"));
    let value = values[position].as_f64().unwrap();
    assert!(
        (value - CONTINUOUS_VALUE).abs() < 1e-9,
        "the stored value is served unchanged: {value}"
    );
}
