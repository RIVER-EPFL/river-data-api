//! A `replicates` input is the measurement: the values typed into the tool are stored as
//! readings of their own parameter at their own index, raw, with the run's standard curve
//! recorded on each row for the database to apply, and the sample statistics the database
//! derives are the numbers the tool displayed.
//!
//! Run: cargo test --test e2e replicates_as_readings -- --test-threads=1

use sea_orm::{ConnectionTrait, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::e2e;

const DOC_PARAM: &str = "00000000-0000-4000-b000-0000000000d1";
const AT: &str = "2025-06-15T11:00:00Z";

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String, String, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO parameters (id, code, name, category, default_units) \
             VALUES ('{DOC_PARAM}', 'DOC', 'DOC', 'measurement', 'ppb')"
        ),
    )
    .await;
    let sensor_id = e2e::create_sensor(&app, &token, DOC_PARAM, "TOC-DOC-1").await;
    let (status, curve) = crate::common::post_json_parse_with_token(
        &app,
        "/api/standard_curves",
        &json!({ "sensor_id": sensor_id, "name": "plate 7", "slope": 1.05, "intercept": -2.0 }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "standard curve ({status}): {curve}");
    (db, app, token, sensor_id, e2e::id_of(&curve))
}

fn reading(sensor_id: &str, curve_id: &str, value: f64, index: i16) -> serde_json::Value {
    json!({
        "parameter_id": DOC_PARAM,
        "sensor_id": sensor_id,
        "standard_curve_id": curve_id,
        "time": AT,
        "value": value,
        "replicate_index": index,
        "input": "DOC",
    })
}

#[tokio::test]
#[serial]
async fn typed_replicates_are_stored_at_their_index_with_the_curve_the_run_applied() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "typed_replicates_are_stored_at_their_index_with_the_curve_the_run_applied",
    )
    .await
    {
        return;
    }
    let (db, app, token, sensor_id, curve_id) = setup().await;
    let site = crate::common::SITE1_ID;

    let (status, run) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &json!({ "DOC": [120.0, null, 118.0], "std_curve": { "standard_curve_id": curve_id } }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "calculate ({status}): {run}");
    let run_id = run["run_id"].as_str().expect("run_id").to_string();
    let shown_avg = run["results"]["DOC_avg_ppb"].as_f64().expect("avg");
    let shown_sd = run["results"]["DOC_sd_ppb"].as_f64().expect("sd");
    // (1.05 * 120 - 2) and (1.05 * 118 - 2), the middle vial a gap.
    assert!((shown_avg - 122.95).abs() < 1e-9, "{shown_avg}");

    let (status, saved) = crate::common::post_json_parse_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": site,
            "tool_run_id": run_id,
            "readings": [
                reading(&sensor_id, &curve_id, 120.0, 0),
                reading(&sensor_id, &curve_id, 118.0, 2),
            ],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "save ({status}): {saved}");
    assert_eq!(saved["inserted"], 2);
    assert_eq!(saved["samples_created"], 1);

    let rows = db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT replicate_index, raw_value, calibrated_value, standard_curve_id::text AS curve, \
                        collection_event_id IS NOT NULL AS attached \
                 FROM readings WHERE site_id = '{site}' AND parameter_id = '{DOC_PARAM}' \
                 ORDER BY replicate_index"
            ),
        ))
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "one reading per typed vial, the gap left unstored");
    for (row, (index, raw)) in rows.iter().zip([(0i16, 120.0f64), (2, 118.0)]) {
        assert_eq!(row.try_get::<i16>("", "replicate_index").unwrap(), index);
        assert_eq!(row.try_get::<f64>("", "raw_value").unwrap(), raw);
        let calibrated = row.try_get::<Option<f64>>("", "calibrated_value").unwrap().unwrap();
        assert!((calibrated - (1.05 * raw - 2.0)).abs() < 1e-9, "{calibrated}");
        assert_eq!(row.try_get::<String>("", "curve").unwrap(), curve_id);
        assert!(row.try_get::<bool>("", "attached").unwrap(), "attached to the visit");
    }

    let sample = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT n, mean, stdev, provenance FROM samples \
                 WHERE site_id = '{site}' AND parameter_id = '{DOC_PARAM}'"
            ),
        ))
        .await
        .unwrap()
        .expect("the vials form one sample");
    assert_eq!(sample.try_get::<i32>("", "n").unwrap(), 2);
    let mean = sample.try_get::<Option<f64>>("", "mean").unwrap().unwrap();
    let stdev = sample.try_get::<Option<f64>>("", "stdev").unwrap().unwrap();
    assert!((mean - shown_avg).abs() < 1e-9, "served {mean}, shown {shown_avg}");
    assert!((stdev - shown_sd).abs() < 1e-9, "served {stdev}, shown {shown_sd}");
    let blob: serde_json::Value = sample.try_get("", "provenance").unwrap();
    assert_eq!(blob["run_id"], run_id);
    assert_eq!(blob["saved_inputs"]["DOC"], DOC_PARAM);
    assert_eq!(blob["inputs"]["DOC"], json!([120.0, null, 118.0]));
}

#[tokio::test]
#[serial]
async fn a_replicate_the_run_did_not_consume_is_refused() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_replicate_the_run_did_not_consume_is_refused",
    )
    .await
    {
        return;
    }
    let (_db, app, token, sensor_id, curve_id) = setup().await;
    let site = crate::common::SITE1_ID;

    let (status, run) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &json!({ "DOC": [120.0, null, 118.0] }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{run}");
    let run_id = run["run_id"].as_str().unwrap();

    for (readings, expected) in [
        // An edited value.
        (vec![reading(&sensor_id, &curve_id, 121.0, 0)], "consumed"),
        // The gap: no value was entered at index 1.
        (vec![reading(&sensor_id, &curve_id, 120.0, 1)], "consumed"),
        // Not a replicates input of this run.
        (
            vec![json!({ "parameter_id": DOC_PARAM, "time": AT, "value": 120.0,
                         "replicate_index": 0, "input": "std_curve" })],
            "not a replicates input",
        ),
    ] {
        let (status, refused) = crate::common::post_json_with_token(
            &app,
            "/api/grab_samples",
            &json!({ "site_id": site, "tool_run_id": run_id, "readings": readings }),
            &token,
        )
        .await;
        assert_eq!(status, 400, "{refused}");
        assert!(refused.contains(expected), "{refused}");
    }

    let (status, refused) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &json!({ "site_id": site, "readings": [reading(&sensor_id, &curve_id, 120.0, 0)] }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "an input without its run: {refused}");
    assert!(refused.contains("tool_run_id"), "{refused}");
}
