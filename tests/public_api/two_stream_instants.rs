//! Scenario: two streams paired to one slot both report at one instant.
//! Expected behaviour: the instant is one served point. A continuous instant is served at the
//! mean of both readings, as the hourly rollup already reports it, and its statistics name both;
//! a spot instant is counted once in the site detail, as it is served once.
//!
//! Run: cargo test --test public_api two_stream_instants -- --test-threads=1

use sea_orm::DatabaseConnection;
use serial_test::serial;
use uuid::Uuid;

const INSTANT: &str = "2025-02-01T10:00:00Z";
const WINDOW: &str = "start=2025-02-01T09:00:00Z&end=2025-02-01T11:00:00Z";
const SITE_URI: &str = "/api/public/test-river/sites/upstream";

async fn setup() -> (DatabaseConnection, axum::Router) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    for sql in [
        format!(
            "UPDATE projects SET is_public = true, public_code = 'test-river' WHERE id = '{}'",
            crate::common::PROJECT_ID
        ),
        format!(
            "UPDATE sites SET public_code = 'upstream' WHERE id = '{}'",
            crate::common::SITE1_ID
        ),
        format!(
            "UPDATE site_parameters SET is_public = true WHERE id = '{}'",
            crate::common::PARAM_S1_TEMP_ID
        ),
    ] {
        crate::common::exec(&db, &sql).await;
    }
    let app = crate::common::build_test_app(db.clone());
    (db, app)
}

/// A stream paired to the temperature slot, reporting `value` at [`INSTANT`].
async fn stream_reporting(db: &DatabaseConnection, kind: &str, value: f64) {
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
                raw_value, measurement_type) \
             VALUES ('{stream_id}', '{}', '{}', '{INSTANT}', 0, {value}, '{kind}')",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
}

fn assert_close(value: Option<f64>, expected: f64, context: &str) {
    let value = value.unwrap_or_else(|| panic!("{context}: missing"));
    assert!((value - expected).abs() < 1e-9, "{context}: {value}");
}

#[tokio::test]
#[serial]
async fn a_continuous_instant_two_streams_feed_is_served_at_their_mean() {
    let (db, app) = setup().await;
    stream_reporting(&db, "continuous", 10.0).await;
    stream_reporting(&db, "continuous", 14.0).await;

    let (status, body) = crate::common::get_json(
        &app,
        &format!("{SITE_URI}/readings?{WINDOW}&include_sample_stats=true"),
    )
    .await;
    assert_eq!(status, 200, "readings ({status}): {body}");
    assert_eq!(
        body["times"].as_array().map(Vec::len),
        Some(1),
        "one served point: {body}"
    );
    let param = &body["parameters"][0];
    // (10.0 + 14.0) / 2
    assert_close(param["values"][0].as_f64(), 12.0, "the served value");
    let stats = &param["sample_stats"];
    assert_eq!(
        stats["n"][0].as_i64(),
        Some(2),
        "both readings pooled: {body}"
    );
    assert_close(stats["mean"][0].as_f64(), 12.0, "the pooled mean");
    // sample sd of {10, 14}, sqrt(8), at the slot's two declared places
    assert_close(stats["sd_sample"][0].as_f64(), 2.83, "the pooled sd");
    assert_close(stats["min"][0].as_f64(), 10.0, "the pooled min");
    assert_close(stats["max"][0].as_f64(), 14.0, "the pooled max");
}

#[tokio::test]
#[serial]
async fn a_spot_instant_two_streams_feed_is_counted_once() {
    let (db, app) = setup().await;
    stream_reporting(&db, "spot", 10.0).await;
    let (status, before) = crate::common::get_json(&app, SITE_URI).await;
    assert_eq!(status, 200, "site detail ({status}): {before}");

    stream_reporting(&db, "spot", 14.0).await;
    let (status, after) = crate::common::get_json(&app, SITE_URI).await;
    assert_eq!(status, 200, "site detail ({status}): {after}");
    assert_eq!(
        after["reading_count"], before["reading_count"],
        "a second stream at a served instant adds no served point"
    );

    let (status, body) = crate::common::get_json(
        &app,
        &format!("{SITE_URI}/readings?{WINDOW}&measurement_type=spot"),
    )
    .await;
    assert_eq!(status, 200, "readings ({status}): {body}");
    assert_eq!(
        body["times"].as_array().map(Vec::len),
        Some(1),
        "one served point: {body}"
    );
}
