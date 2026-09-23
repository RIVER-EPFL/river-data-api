//! End-to-end observability surfaces: ingest device-health status events and read them back
//! (US-2.1/2.3), cross-entity search (US-9.2), CSV/NDJSON export of readings (US-8.1), and grabs
//! tagged alongside continuous readings (US-8.2). The comparison export the same story used to
//! drive is covered, with its edges, by `tools_grab_export.rs`.
//!
//! Run: cargo test --test e2e -- --test-threads=1

use crate::common::e2e;
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
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["inserted"],
        2
    );

    // US-2.3: read the device-health timeline back (CSV form, as the UI timeline does).
    let uri = format!(
        "/api/sites/{site1}/status_events?start=2025-01-15T00:00:00Z&end=2025-01-16T00:00:00Z&format=csv"
    );
    let (status, csv) = crate::common::get_csv_with_token(&app, &uri, &token).await;
    assert_eq!(status, 200, "status_events csv ({status})");
    assert!(
        csv.contains("online") && csv.contains("low_battery"),
        "status timeline should contain both events:\n{csv}"
    );

    // US-9.2: cross-entity search finds the seeded Dissolved_O2 parameter.
    let (status, search) =
        crate::common::get_json_with_token(&app, "/api/search?q=Dissolved", &token).await;
    assert_eq!(status, 200, "search ({status}): {search}");
    let params = search["results"]["parameters"]
        .as_array()
        .expect("search results.parameters");
    assert!(
        params
            .iter()
            .any(|p| p["name"].as_str().is_some_and(|n| n.contains("Dissolved"))),
        "search should match Dissolved_O2: {search}"
    );

    // US-8.1: export readings as CSV and NDJSON.
    let rd =
        format!("/api/sites/{site1}/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T01:00:00Z");
    let (status, csv) =
        crate::common::get_csv_with_token(&app, &format!("{rd}&format=csv"), &token).await;
    assert_eq!(status, 200, "readings csv ({status})");
    assert!(
        csv.lines().filter(|l| !l.is_empty()).count() > 1,
        "CSV export should have a header + rows:\n{csv}"
    );

    let (status, ndjson) =
        crate::common::get_ndjson_with_token(&app, &format!("{rd}&format=ndjson"), &token).await;
    assert_eq!(status, 200, "readings ndjson ({status})");
    let lines: Vec<&str> = ndjson.lines().filter(|l| !l.is_empty()).collect();
    assert!(!lines.is_empty(), "NDJSON export should have lines");
    assert!(
        serde_json::from_str::<serde_json::Value>(lines[0]).is_ok(),
        "each NDJSON line is valid JSON: {}",
        lines[0]
    );
}

/// US-8.2 (CNET/METALP port): grab samples coexist with continuous readings, tagged by
/// `measurement_type` so they render and filter distinctly, on the private readings arm in JSON
/// and CSV. The comparison export itself is `tools_grab_export.rs`.
#[tokio::test]
#[serial]
async fn grabs_are_tagged_alongside_continuous_readings() {
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
            "site_id": site1,
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
    assert!(
        mtypes.contains(&Some("spot")),
        "grab replicates should be tagged 'spot': {readings}"
    );

    // Filtering to grabs only returns the three grab replicates.
    let spot_uri = format!(
        "/api/sites/{site1}/readings?parameter_ids={dop}&start=2025-01-15T06:00:00Z&end=2025-01-15T06:10:00Z&include_replicates=true&measurement_type=spot"
    );
    let (status, spot) = crate::common::get_json_with_token(&app, &spot_uri, &token).await;
    assert_eq!(status, 200, "spot readings ({status}): {spot}");
    assert_eq!(
        e2e::values_for(&spot, dop).len(),
        3,
        "measurement_type=spot returns the grab replicates: {spot}"
    );

    // CSV export carries the {name}_measurement_type column with the grab tagged 'spot'.
    let (status, csv) =
        crate::common::get_csv_with_token(&app, &format!("{rd}&format=csv"), &token).await;
    assert_eq!(status, 200, "readings csv ({status})");
    assert!(
        csv.lines()
            .next()
            .is_some_and(|h| h.contains("_measurement_type")),
        "CSV header should include the measurement_type column:\n{csv}"
    );
    assert!(
        csv.contains("spot"),
        "CSV should tag the grab row 'spot':\n{csv}"
    );
}
