//! A spot instant is served as its replicate mean, and with `include_sample_stats=true` the
//! public arm says so: n, mean, the sample sd under `sd_sample`, min and max per instant. Without
//! the annotation the body is unchanged.
//!
//! Run: cargo test --test public_api sample_stats_annotation -- --test-threads=1

use sea_orm::DatabaseConnection;
use serial_test::serial;
use uuid::Uuid;

const SPOT_TIME: &str = "2025-01-15T00:05:30Z";
const CONTINUOUS_TIME: &str = "2025-01-15T00:07:30Z";
const WINDOW: &str = "start=2025-01-15T00:00:00Z&end=2025-01-15T01:00:00Z";
const READINGS_URI: &str = "/api/public/test-river/sites/upstream/readings";
const REPLICATES: [f64; 3] = [10.0, 20.0, 30.0];

/// A public project with one exposed slot holding a three-replicate spot group behind a sample
/// row and one continuous reading.
async fn setup() -> (DatabaseConnection, axum::Router) {
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
             VALUES ('{stream_id}', 'statsrc', '{}', true)",
            Uuid::new_v4()
        ),
    )
    .await;
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
    for (idx, value) in REPLICATES.iter().enumerate() {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                    raw_value, measurement_type, sample_id) \
                 VALUES ('{stream_id}', '{site}', '{param}', '{SPOT_TIME}', {idx}, {value}, \
                         'spot', '{sample_id}')",
                site = crate::common::SITE1_ID,
                param = crate::common::GLOBAL_PARAM_TEMP_ID,
            ),
        )
        .await;
    }
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, replicate_index, \
                raw_value, measurement_type) \
             VALUES ('{stream_id}', '{site}', '{param}', '{CONTINUOUS_TIME}', 0, 13.75, \
                     'continuous')",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TEMP_ID,
        ),
    )
    .await;

    let app = crate::common::build_test_app(db.clone());
    (db, app)
}

fn assert_close(value: f64, expected: f64, context: &str) {
    assert!((value - expected).abs() < 1e-9, "{context}: {value}");
}

/// The parameter block for the spot instant's position and the continuous reading's position.
fn spot_and_continuous_positions(body: &serde_json::Value) -> (usize, usize) {
    let times = body["times"].as_array().unwrap();
    let at = |t: &str| {
        times
            .iter()
            .position(|v| v.as_str() == Some(t))
            .unwrap_or_else(|| panic!("{t} served: {body}"))
    };
    (at("2025-01-15 00:05:30"), at("2025-01-15 00:07:30"))
}

#[tokio::test]
#[serial]
async fn without_the_annotation_the_body_is_unchanged() {
    let (_db, app) = setup().await;
    let (status, body) = crate::common::get_json(&app, &format!("{READINGS_URI}?{WINDOW}")).await;
    assert_eq!(status, 200, "{body}");
    let param = &body["parameters"][0];
    assert!(
        param.get("sample_stats").is_none(),
        "statistics are opt-in: {param}"
    );
    let (spot, _) = spot_and_continuous_positions(&body);
    assert_close(
        param["values"][spot].as_f64().unwrap(),
        20.0,
        "the served value is the replicate mean",
    );
}

#[tokio::test]
#[serial]
async fn the_annotation_publishes_n_and_the_sample_sd() {
    let (_db, app) = setup().await;
    let (status, body) = crate::common::get_json(
        &app,
        &format!("{READINGS_URI}?{WINDOW}&include_sample_stats=true"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let param = &body["parameters"][0];
    let stats = &param["sample_stats"];
    assert!(stats.get("sd_estimator").is_none(), "{param}");
    assert!(
        stats.get("sd").is_none(),
        "the sd is named for its divisor: {param}"
    );
    let (spot, cont) = spot_and_continuous_positions(&body);

    assert_eq!(stats["n"][spot], 3, "{stats}");
    assert_close(stats["mean"][spot].as_f64().unwrap(), 20.0, "mean");
    // Sample sd of 10, 20, 30: sqrt(200 / 2)
    assert_close(
        stats["sd_sample"][spot].as_f64().unwrap(),
        10.0,
        "sample sd",
    );
    assert_close(stats["min"][spot].as_f64().unwrap(), 10.0, "min");
    assert_close(stats["max"][spot].as_f64().unwrap(), 30.0, "max");

    assert_eq!(
        stats["n"][cont], 1,
        "a continuous reading is one measurement: {stats}"
    );
    assert!(stats["sd_sample"][cont].is_null(), "{stats}");
    assert!(stats["mean"][cont].is_null(), "{stats}");
}

#[tokio::test]
#[serial]
async fn csv_and_ndjson_carry_the_statistics_columns() {
    let (_db, app) = setup().await;
    let (status, csv) = crate::common::get(
        &app,
        &format!("{READINGS_URI}?{WINDOW}&include_sample_stats=true&format=csv"),
    )
    .await;
    assert_eq!(status, 200, "{csv}");
    let mut lines = csv.lines().filter(|l| !l.trim().is_empty());
    let header: Vec<&str> = lines.next().unwrap().split(',').collect();
    for column in [
        "DO_Temperature",
        "DO_Temperature_n",
        "DO_Temperature_mean",
        "DO_Temperature_sd_sample",
        "DO_Temperature_min",
        "DO_Temperature_max",
    ] {
        assert!(header.contains(&column), "{column} in {header:?}");
    }
    for column in ["DO_Temperature_sd", "DO_Temperature_sd_estimator"] {
        assert!(!header.contains(&column), "{column} not in {header:?}");
    }
    let col = |name: &str| header.iter().position(|h| *h == name).unwrap();
    let spot_row: Vec<&str> = lines
        .clone()
        .find(|l| l.starts_with("2025-01-15 00:05:30"))
        .unwrap_or_else(|| panic!("spot row: {csv}"))
        .split(',')
        .collect();
    assert_eq!(spot_row[col("DO_Temperature_n")], "3", "{csv}");
    assert_eq!(spot_row[col("DO_Temperature_sd_sample")], "10", "{csv}");
    let cont_row: Vec<&str> = lines
        .find(|l| l.starts_with("2025-01-15 00:07:30"))
        .unwrap_or_else(|| panic!("continuous row: {csv}"))
        .split(',')
        .collect();
    assert_eq!(cont_row[col("DO_Temperature_n")], "1", "{csv}");
    assert_eq!(cont_row[col("DO_Temperature_sd_sample")], "", "{csv}");

    let (status, ndjson) = crate::common::get(
        &app,
        &format!("{READINGS_URI}?{WINDOW}&include_sample_stats=true&format=ndjson"),
    )
    .await;
    assert_eq!(status, 200, "{ndjson}");
    let spot: serde_json::Value = ndjson
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .find(|v: &serde_json::Value| v["time"] == "2025-01-15 00:05:30")
        .unwrap_or_else(|| panic!("spot line: {ndjson}"));
    assert_eq!(spot["DO_Temperature_n"], 3, "{spot}");
    assert_close(
        spot["DO_Temperature_sd_sample"].as_f64().unwrap(),
        10.0,
        "ndjson sd",
    );
    assert!(spot.get("DO_Temperature_sd_estimator").is_none(), "{spot}");
}

#[tokio::test]
#[serial]
async fn the_annotation_is_part_of_the_cache_key() {
    let (_db, app) = setup().await;
    let plain = format!("{READINGS_URI}?{WINDOW}");
    let annotated = format!("{plain}&include_sample_stats=true");
    let (_, first) = crate::common::get_json(&app, &plain).await;
    assert!(first["parameters"][0].get("sample_stats").is_none());
    let (_, second) = crate::common::get_json(&app, &annotated).await;
    assert!(
        second["parameters"][0].get("sample_stats").is_some(),
        "the annotated request is not served from the plain entry: {second}"
    );
    let (_, third) = crate::common::get_json(&app, &plain).await;
    assert!(
        third["parameters"][0].get("sample_stats").is_none(),
        "the plain request is not served from the annotated entry: {third}"
    );
}
