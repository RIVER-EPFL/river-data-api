//! A spot instant is served as its sample statistics: the mean over unflagged replicates where a
//! sample row exists, the lowest unflagged replicate's own value where none does (unpaired stream
//! or not yet materialised). Flagging a replicate therefore moves the served value rather than
//! removing the instant or handing it to a different replicate; only a fully flagged group is
//! absent. A continuous series, whose instants hold a single row at replicate index 0, is served
//! exactly as stored.
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
/// single continuous reading at another. `with_sample` decides whether the spot group is behind a
/// trigger-maintained sample row (the paired, materialised case) or bare (the fallback case).
async fn setup(with_sample: bool) -> (DatabaseConnection, axum::Router, Uuid) {
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

    let sample_ref = if with_sample {
        let sample_id = Uuid::new_v4();
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO samples (id, site_id, parameter_id, collected_at) \
                 VALUES ('{sample_id}', '{}', '{}', '{SPOT_TIME}')",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TEMP_ID
            ),
        )
        .await;
        format!("'{sample_id}'")
    } else {
        "NULL".to_string()
    };

    for (time, idx, value, kind, sample) in [
        (SPOT_TIME, 0, 10.0, "spot", sample_ref.as_str()),
        (SPOT_TIME, 1, 20.0, "spot", sample_ref.as_str()),
        (CONTINUOUS_TIME, 0, CONTINUOUS_VALUE, "continuous", "NULL"),
    ] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                    raw_value, measurement_type, sample_id) \
                 VALUES ('{stream_id}', '{site}', '{param}', '{time}', {idx}, {value}, '{kind}', \
                         {sample})",
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

fn assert_close(value: f64, expected: f64, context: &str) {
    assert!((value - expected).abs() < 1e-9, "{context}: {value}");
}

#[tokio::test]
#[serial]
async fn flagging_a_replicate_recomputes_the_sample_mean() {
    let (db, app, stream) = setup(true).await;
    assert_close(
        served_spot_value(&app).await.expect("instant served"),
        15.0,
        "the served value is the sample mean",
    );

    set_flag(&db, stream, 0, true).await;
    assert_close(
        served_spot_value(&app).await.expect("instant still served"),
        20.0,
        "the mean recomputes over the unflagged remainder",
    );

    set_flag(&db, stream, 1, true).await;
    assert_eq!(
        served_spot_value(&app).await,
        None,
        "a fully flagged group serves nothing"
    );
}

#[tokio::test]
#[serial]
async fn without_a_sample_the_lowest_unflagged_replicate_serves() {
    let (db, app, stream) = setup(false).await;
    assert_close(
        served_spot_value(&app).await.expect("instant served"),
        10.0,
        "the lowest unflagged replicate is the fallback value",
    );

    set_flag(&db, stream, 0, true).await;
    assert_close(
        served_spot_value(&app).await.expect("instant still served"),
        20.0,
        "flagging the lowest index moves the fallback to the next unflagged replicate",
    );

    set_flag(&db, stream, 1, true).await;
    assert_eq!(
        served_spot_value(&app).await,
        None,
        "a fully flagged group serves nothing"
    );
}

#[tokio::test]
#[serial]
async fn a_continuous_reading_is_served_as_stored() {
    let (db, app, _stream) = setup(false).await;

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
