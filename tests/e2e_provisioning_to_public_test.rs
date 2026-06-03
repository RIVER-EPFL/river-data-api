//! End-to-end workflow: provision a project/site/parameter from scratch, assign a parameter to the
//! site with ONLY {site_id, parameter_id} (exercising the on_create defaults + name backfill),
//! register and pair a data stream, ingest readings through the stream, then verify the readings,
//! the tracked aggregate refresh, and the public API surface (JSON + CSV).
//!
//! Complements `public_workflow_e2e_test.rs` (which drives the CSV-import path) by exercising the
//! stream register → pair → ingest path and the WS1 minimal-assignment friction fix.
//!
//! Run: cargo test --test e2e_provisioning_to_public_test -- --test-threads=1

mod common;

use common::e2e;
use serial_test::serial;

#[tokio::test]
#[serial]
async fn provision_pair_ingest_and_expose_publicly() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());

    // 1. Provision project → site → global parameter.
    let project_id = e2e::create_project(&app, &token, "E2E Prov", "e2e_prov", true).await;
    let site_id = e2e::create_site(&app, &token, &project_id, "Prov Station", "prov_station").await;
    let param_id = e2e::create_parameter(&app, &token, "e2e_depth", "E2E Depth", "mm").await;

    // 2. Assign the parameter to the site with ONLY {site_id, parameter_id} (WS1 friction fix):
    //    name is backfilled from the parameter, sensor_type defaults to "".
    let sp_id = e2e::assign_site_parameter_minimal(&app, &token, &site_id, &param_id).await;
    let (status, sp) =
        common::get_json_with_token(&app, &format!("/api/site_parameters/{sp_id}"), &token).await;
    assert_eq!(status, 200, "get site_parameter: {sp}");
    assert_eq!(sp["name"], "E2E Depth", "name should be backfilled from the parameter: {sp}");
    assert_eq!(sp["sensor_type"], "", "sensor_type should default to empty: {sp}");

    // 3. Register a source-agnostic stream and pair it to the site_parameter.
    let (status, stream) = common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &serde_json::json!({ "source_system": "e2e", "source_key": "e2e-prov-1", "source_name": "Prov depth" }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "register stream ({status}): {stream}");
    let stream_id = e2e::id_of(&stream);

    let (status, paired) = common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &serde_json::json!({ "site_parameter_id": sp_id }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "pair stream ({status}): {paired}");

    // 4. Ingest readings through the paired stream — they should be stamped with site_id/parameter_id.
    let times: Vec<String> = (0..6).map(|i| format!("2025-06-01T00:{:02}:00Z", i * 10)).collect();
    let raw: Vec<f64> = vec![10.0, 11.0, 12.0, 13.0, 14.0, 15.0];
    let readings_payload: Vec<serde_json::Value> = times
        .iter()
        .zip(&raw)
        .map(|(t, v)| serde_json::json!({ "time": t, "raw_value": v }))
        .collect();
    let (status, ing) = common::post_json_with_token(
        &app,
        "/api/ingest",
        &serde_json::json!({ "stream_id": stream_id, "readings": readings_payload }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "ingest ({status}): {ing}");
    let ing: serde_json::Value = serde_json::from_str(&ing).unwrap();
    assert_eq!(ing["inserted"].as_u64().unwrap(), 6, "ingest inserted: {ing}");
    assert_eq!(ing["paired"], true, "stream should be paired: {ing}");

    // 5. Authenticated readings reproduce the ingested series (COALESCE(calibrated, raw)).
    let readings_uri =
        format!("/api/sites/{site_id}/readings?start=2025-06-01T00:00:00Z&end=2025-06-01T00:59:00Z");
    let (status, readings) = common::get_json_with_token(&app, &readings_uri, &token).await;
    assert_eq!(status, 200, "readings ({status}): {readings}");
    let got = e2e::values_for(&readings, &param_id);
    assert_eq!(got.len(), 6, "expected 6 readings, got {}: {readings}", got.len());
    for (i, v) in raw.iter().enumerate() {
        assert!((got[i] - v).abs() < 1e-6, "reading[{i}] {} != {v}", got[i]);
    }

    // 6. Refresh continuous aggregates as a TRACKED job (WS3), then assert the hourly mean.
    let (status, refresh) = common::post_json_with_token(
        &app,
        "/api/actions/refresh_aggregates",
        &serde_json::json!({ "full": true }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "refresh_aggregates ({status}): {refresh}");
    let refresh: serde_json::Value = serde_json::from_str(&refresh).unwrap();
    let job_id = refresh["job_id"].as_str().expect("refresh_aggregates should return a job_id");
    let final_status = e2e::poll_job(&app, &token, job_id, 30).await;
    assert_eq!(final_status, "completed", "aggregate refresh job should complete");

    let agg_uri = format!(
        "/api/sites/{site_id}/aggregates/hourly?start=2025-06-01T00:00:00Z&end=2025-06-01T00:59:00Z"
    );
    let (status, agg) = common::get_json_with_token(&app, &agg_uri, &token).await;
    assert_eq!(status, 200, "aggregates ({status}): {agg}");
    let avg = e2e::field_for(&agg, &param_id, "avg");
    assert!(
        avg.first().is_some_and(|v| (v - 12.5).abs() < 1e-6),
        "hourly avg should be 12.5 (mean of 10..15), got {avg:?}"
    );

    // 7. Expose publicly and verify the public JSON + CSV surfaces reproduce the data.
    e2e::set_site_parameter_public(&db, &sp_id).await;

    let pub_uri = format!(
        "/api/public/e2e_prov/sites/{site_id}/readings?start=2025-06-01T00:00:00Z&end=2025-06-01T00:59:00Z"
    );
    let (status, pub_readings) = common::get_json(&app, &pub_uri).await;
    assert_eq!(status, 200, "public readings ({status}): {pub_readings}");
    // Public readings key parameters by the global parameter short code (not site_parameter / id).
    let pub_got = e2e::values_for(&pub_readings, "e2e_depth");
    assert_eq!(pub_got.len(), 6, "public should expose 6 readings: {pub_readings}");

    let (status, csv) = common::get_csv_with_token(&app, &format!("{pub_uri}&format=csv"), &token).await;
    assert_eq!(status, 200, "public CSV ({status})");
    let lines: Vec<&str> = csv.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 7, "CSV should have a header + 6 rows, got {}:\n{csv}", lines.len());
    assert!(lines[0].to_lowercase().contains("time"), "CSV header should include a time column: {}", lines[0]);
}
