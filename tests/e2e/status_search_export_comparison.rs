//! End-to-end observability surfaces: ingest device-health status events and read them back
//! (US-2.1/2.3), cross-entity search (US-9.2), CSV/NDJSON export of readings (US-8.1), and the
//! sensor-vs-grab comparison export (US-8.2, ported from CNET/METALP).
//!
//! Run: cargo test --test e2e -- --test-threads=1


use crate::common::e2e;
use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;

#[tokio::test]
#[serial]
async fn status_events_search_and_export() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let site1 = crate::common::SITE1_ID;

    // US-2.1: ingest device-health status events via the batch endpoint.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/status_events/batch",
        &serde_json::json!({
            "events": [
                { "site_id": site1, "parameter_id": crate::common::GLOBAL_PARAM_TURB_ID, "time": "2025-01-15T06:00:00Z", "value": "online" },
                { "site_id": site1, "parameter_id": crate::common::GLOBAL_PARAM_TURB_ID, "time": "2025-01-15T07:00:00Z", "value": "low_battery" },
            ],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "status_events/batch ({status}): {body}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["inserted"], 2);

    // US-2.3: read the device-health timeline back (CSV form, as the UI timeline does).
    let uri = format!("/api/sites/{site1}/status_events?start=2025-01-15T00:00:00Z&end=2025-01-16T00:00:00Z&format=csv");
    let (status, csv) = crate::common::get_csv_with_token(&app, &uri, &token).await;
    assert_eq!(status, 200, "status_events csv ({status})");
    assert!(csv.contains("online") && csv.contains("low_battery"), "status timeline should contain both events:\n{csv}");

    // US-9.2: cross-entity search finds the seeded Dissolved_O2 parameter.
    let (status, search) = crate::common::get_json_with_token(&app, "/api/search?q=Dissolved", &token).await;
    assert_eq!(status, 200, "search ({status}): {search}");
    let params = search["results"]["parameters"].as_array().expect("search results.parameters");
    assert!(
        params.iter().any(|p| p["name"].as_str().is_some_and(|n| n.contains("Dissolved"))),
        "search should match Dissolved_O2: {search}"
    );

    // US-8.1: export readings as CSV and NDJSON.
    let rd = format!("/api/sites/{site1}/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T01:00:00Z");
    let (status, csv) = crate::common::get_csv_with_token(&app, &format!("{rd}&format=csv"), &token).await;
    assert_eq!(status, 200, "readings csv ({status})");
    assert!(csv.lines().filter(|l| !l.is_empty()).count() > 1, "CSV export should have a header + rows:\n{csv}");

    let (status, ndjson) = crate::common::get_ndjson_with_token(&app, &format!("{rd}&format=ndjson"), &token).await;
    assert_eq!(status, 200, "readings ndjson ({status})");
    let lines: Vec<&str> = ndjson.lines().filter(|l| !l.is_empty()).collect();
    assert!(!lines.is_empty(), "NDJSON export should have lines");
    assert!(serde_json::from_str::<serde_json::Value>(lines[0]).is_ok(), "each NDJSON line is valid JSON: {}", lines[0]);
}

/// US-8.2 (CNET/METALP port): grab samples coexist with continuous readings — tagged by
/// `measurement_type` so they render/filter distinctly — and the comparison export pairs each grab
/// with the continuous sensor average over the post-grab window [T+2h, T+6h].
#[tokio::test]
#[serial]
async fn sensor_vs_grab_comparison_export() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await; // continuous readings at SITE1 for DO, 10-min cadence from 2025-01-15
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let site1 = crate::common::SITE1_ID;
    let dop = crate::common::GLOBAL_PARAM_DO_ID;
    let grab_time = "2025-01-15T06:05:00Z"; // off the 10-min grid so it never collides with a sensor point

    // A grab sample with three replicates → one `samples` row (mean 9.2) + three 'spot' readings.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": site1, "created_by": "e2e",
            "readings": [
                { "parameter_id": dop, "value": 9.0, "time": grab_time },
                { "parameter_id": dop, "value": 9.2, "time": grab_time },
                { "parameter_id": dop, "value": 9.4, "time": grab_time },
            ],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab_samples ({status}): {body}");

    // Grab replicates coexist with continuous points and are tagged 'spot' when the indicator is
    // requested. (A multi-replicate grab lives at replicate_index 1..n, so surface it with
    // include_replicates; its mean lives in `samples` and drives the comparison export below.)
    let rd = format!(
        "/api/sites/{site1}/readings?parameter_ids={dop}&start=2025-01-15T06:00:00Z&end=2025-01-15T06:10:00Z&include_replicates=true&include_measurement_type=true"
    );
    let (status, readings) = crate::common::get_json_with_token(&app, &rd, &token).await;
    assert_eq!(status, 200, "readings ({status}): {readings}");
    let do_param = readings["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["parameter_id"].as_str() == Some(dop))
        .expect("DO parameter in readings");
    let mtypes: Vec<Option<&str>> = do_param["measurement_types"]
        .as_array()
        .expect("measurement_types present when include_measurement_type=true")
        .iter()
        .map(|v| v.as_str())
        .collect();
    assert!(mtypes.contains(&Some("spot")), "grab replicates should be tagged 'spot': {readings}");

    // Filtering to grabs only returns the three grab replicates.
    let spot_uri = format!(
        "/api/sites/{site1}/readings?parameter_ids={dop}&start=2025-01-15T06:00:00Z&end=2025-01-15T06:10:00Z&include_replicates=true&measurement_type=spot"
    );
    let (status, spot) = crate::common::get_json_with_token(&app, &spot_uri, &token).await;
    assert_eq!(status, 200, "spot readings ({status}): {spot}");
    assert_eq!(e2e::values_for(&spot, dop).len(), 3, "measurement_type=spot returns the grab replicates: {spot}");

    // CSV export carries the {name}_measurement_type column with the grab tagged 'spot'.
    let (status, csv) = crate::common::get_csv_with_token(&app, &format!("{rd}&format=csv"), &token).await;
    assert_eq!(status, 200, "readings csv ({status})");
    assert!(
        csv.lines().next().is_some_and(|h| h.contains("_measurement_type")),
        "CSV header should include the measurement_type column:\n{csv}"
    );
    assert!(csv.contains("spot"), "CSV should tag the grab row 'spot':\n{csv}");

    // The comparison export pairs the grab with the continuous average over [T+2h, T+6h].
    let exp = format!(
        "/api/sites/{site1}/export/sensor-vs-grab?parameter_id={dop}&start=2025-01-15T00:00:00Z&end=2025-01-15T23:59:59Z"
    );
    let (status, cmp) = crate::common::get_json_with_token(&app, &exp, &token).await;
    assert_eq!(status, 200, "sensor-vs-grab ({status}): {cmp}");
    let rows = cmp["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1, "exactly one grab in range: {cmp}");
    let row = &rows[0];
    assert!((row["grab_value"].as_f64().unwrap() - 9.2).abs() < 1e-9, "grab_value is the replicate mean: {row}");
    let sensor_n = row["sensor_n"].as_i64().unwrap();
    assert!(sensor_n > 0, "continuous readings should fall in the +2-6h window: {row}");
    let sensor_avg = row["sensor_avg"].as_f64().expect("sensor_avg present");

    // Cross-check sensor_avg against a direct query over the same window + continuous filter.
    let expected_avg: f64 = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT avg(COALESCE(calibrated_value, raw_value)) AS a FROM readings \
                 WHERE site_id='{site1}' AND parameter_id='{dop}' \
                   AND measurement_type IS DISTINCT FROM 'spot' AND measurement_type IS DISTINCT FROM 'derived' \
                   AND time >= '2025-01-15T08:05:00Z' AND time <= '2025-01-15T12:05:00Z'"
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "a")
        .unwrap();
    assert!((sensor_avg - expected_avg).abs() < 1e-6, "sensor_avg {sensor_avg} should match window average {expected_avg}");
    assert!(
        (row["difference"].as_f64().unwrap() - (9.2 - sensor_avg)).abs() < 1e-6,
        "difference should be grab − sensor_avg: {row}"
    );

    // CSV form of the comparison: header + one row.
    let (status, ccsv) = crate::common::get_csv_with_token(&app, &format!("{exp}&format=csv"), &token).await;
    assert_eq!(status, 200, "comparison csv ({status})");
    let clines: Vec<&str> = ccsv.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(clines.len(), 2, "comparison CSV: header + one row:\n{ccsv}");
    assert!(
        clines[0].contains("grab_value") && clines[0].contains("sensor_avg") && clines[0].contains("difference"),
        "comparison CSV header columns: {}",
        clines[0]
    );
}
