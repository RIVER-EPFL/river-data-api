//! End-to-end observability surfaces: ingest device-health status events and read them back
//! (US-2.1/2.3), cross-entity search (US-9.2), and CSV/NDJSON export of readings (US-8.1). Plus an
//! aspirational, currently-blocked sensor-vs-grab comparison export (US-8.2).
//!
//! Run: cargo test --test e2e_observability_test -- --test-threads=1

mod common;

use serial_test::serial;

#[tokio::test]
#[serial]
async fn status_events_search_and_export() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());

    let site1 = common::SITE1_ID;

    // US-2.1: ingest device-health status events via the batch endpoint.
    let (status, body) = common::post_json_with_token(
        &app,
        "/api/status_events/batch",
        &serde_json::json!({
            "events": [
                { "site_id": site1, "parameter_id": common::GLOBAL_PARAM_TURB_ID, "time": "2025-01-15T06:00:00Z", "value": "online" },
                { "site_id": site1, "parameter_id": common::GLOBAL_PARAM_TURB_ID, "time": "2025-01-15T07:00:00Z", "value": "low_battery" },
            ],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "status_events/batch ({status}): {body}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["inserted"], 2);

    // US-2.3: read the device-health timeline back (CSV form, as the UI timeline does).
    let uri = format!("/api/sites/{site1}/status_events?start=2025-01-15T00:00:00Z&end=2025-01-16T00:00:00Z&format=csv");
    let (status, csv) = common::get_csv_with_token(&app, &uri, &token).await;
    assert_eq!(status, 200, "status_events csv ({status})");
    assert!(csv.contains("online") && csv.contains("low_battery"), "status timeline should contain both events:\n{csv}");

    // US-9.2: cross-entity search finds the seeded Dissolved_O2 parameter.
    let (status, search) = common::get_json_with_token(&app, "/api/search?q=Dissolved", &token).await;
    assert_eq!(status, 200, "search ({status}): {search}");
    let params = search["results"]["parameters"].as_array().expect("search results.parameters");
    assert!(
        params.iter().any(|p| p["name"].as_str().is_some_and(|n| n.contains("Dissolved"))),
        "search should match Dissolved_O2: {search}"
    );

    // US-8.1: export readings as CSV and NDJSON.
    let rd = format!("/api/sites/{site1}/readings?start=2025-01-15T00:00:00Z&end=2025-01-15T01:00:00Z");
    let (status, csv) = common::get_csv_with_token(&app, &format!("{rd}&format=csv"), &token).await;
    assert_eq!(status, 200, "readings csv ({status})");
    assert!(csv.lines().filter(|l| !l.is_empty()).count() > 1, "CSV export should have a header + rows:\n{csv}");

    let (status, ndjson) = common::get_ndjson_with_token(&app, &format!("{rd}&format=ndjson"), &token).await;
    assert_eq!(status, 200, "readings ndjson ({status})");
    let lines: Vec<&str> = ndjson.lines().filter(|l| !l.is_empty()).collect();
    assert!(!lines.is_empty(), "NDJSON export should have lines");
    assert!(serde_json::from_str::<serde_json::Value>(lines[0]).is_ok(), "each NDJSON line is valid JSON: {}", lines[0]);
}

/// Aspirational (US-8.2): a sensor-vs-grab comparison export that time-aligns continuous readings to
/// the nearest grab sample. BLOCKED — there is no such endpoint; exports are per-site readings only.
#[tokio::test]
#[serial]
#[ignore = "BLOCKED: no sensor-vs-grab comparison export endpoint (US-8.2)"]
async fn sensor_vs_grab_comparison_export() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    common::seed_test_data(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());

    // Intended: GET an export that pairs each grab sample with the nearest continuous reading and a
    // computed difference column. No endpoint exists, so this 404s today.
    let uri = format!(
        "/api/sites/{}/export/sensor-vs-grab?parameter_id={}&start=2025-01-15T00:00:00Z&end=2025-01-16T00:00:00Z",
        common::SITE1_ID, common::GLOBAL_PARAM_DO_ID
    );
    let (status, _body) = common::get_with_token(&app, &uri, &token).await;
    assert!((200..300).contains(&status), "sensor-vs-grab export should exist (got {status})");
}
