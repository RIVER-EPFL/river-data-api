//! End-to-end: build a project from an empty database entirely through the API, ingest historical
//! data in the client's wide-CSV format, expose parameters publicly, and assert the public endpoints
//! reproduce the data — including the dynamically derived parameter and the continuous aggregates.
//!
//! This mirrors the production migration/cutover: fresh schema → configure entities → ingest CSV →
//! serve the public contract. No SQL seed; every entity is created via its real endpoint.
//!
//! Run with: cargo test --test public_workflow_e2e_test

mod common;

use serial_test::serial;
use std::time::{Duration, Instant};

/// Real, full-precision Verbier readings exported from the production database (2026-03-01,
/// 10-minute interval). DOuM → dissolved_oxygen, WaterTempdegC → temperature; DOmgL is recomputed
/// from DOuM, not ingested.
const CSV: &str = "DateTime,DOuM,WaterTempdegC\n\
2026-03-01 00:00:00,317.9781494140625,4.395852565765381\n\
2026-03-01 00:10:00,318.89630126953125,4.352995872497559\n\
2026-03-01 00:20:00,317.3660888671875,4.2978949546813965\n\
2026-03-01 00:30:00,319.8144226074219,4.346873760223389\n\
2026-03-01 00:40:00,319.6614074707031,4.322384357452393\n\
2026-03-01 00:50:00,319.20233154296875,4.322384357452393\n\
2026-03-01 01:00:00,320.2734680175781,4.304017543792725\n\
2026-03-01 01:10:00,319.6614074707031,4.3407511711120605\n\
2026-03-01 01:20:00,319.50836181640625,4.3101396560668945\n\
2026-03-01 01:30:00,319.20233154296875,4.3101396560668945\n\
2026-03-01 01:40:00,321.03857421875,4.2795281410217285\n\
2026-03-01 01:50:00,321.03857421875,4.2734055519104\n\
2026-03-01 02:00:00,321.3446350097656,4.248916149139404\n\
2026-03-01 02:10:00,321.8036804199219,4.19993782043457\n\
2026-03-01 02:20:00,321.19158935546875,4.2795281410217285\n\
2026-03-01 02:30:00,320.7325439453125,4.463198184967041\n\
2026-03-01 02:40:00,321.6506652832031,4.212182521820068\n\
2026-03-01 02:50:00,321.6506652832031,4.175448417663574\n\
2026-03-01 03:00:00,322.415771484375,4.163203716278076\n\
2026-03-01 03:10:00,322.415771484375,4.157081604003906\n\
2026-03-01 03:20:00,322.56878662109375,4.144836902618408\n\
2026-03-01 03:30:00,314.6116943359375,5.124410152435303\n\
2026-03-01 03:40:00,321.9566955566406,4.2366719245910645\n\
2026-03-01 03:50:00,322.56878662109375,4.181570529937744\n";

/// Parse the embedded CSV into (DateTime, DOuM, WaterTempdegC) so assertions track the real data.
fn parsed_csv() -> Vec<(String, f64, f64)> {
    CSV.lines()
        .skip(1)
        .filter(|l| !l.is_empty())
        .map(|l| {
            let c: Vec<&str> = l.split(',').collect();
            (c[0].to_string(), c[1].parse().unwrap(), c[2].parse().unwrap())
        })
        .collect()
}

/// Extract a numeric array (`field`) for a named parameter. Readings use `values`; aggregates
/// expose `avg`/`min`/`max`/`count`.
fn field_for(resp: &serde_json::Value, name: &str, field: &str) -> Vec<f64> {
    let params = resp["parameters"]
        .as_array()
        .unwrap_or_else(|| panic!("no 'parameters' array in response: {resp}"));
    let p = params
        .iter()
        .find(|p| p["name"] == name)
        .unwrap_or_else(|| panic!("parameter {name} missing in {resp}"));
    p[field]
        .as_array()
        .unwrap_or_else(|| panic!("'{field}' not an array for {name}: param={p}"))
        .iter()
        .map(|v| v.as_f64().unwrap_or(f64::NAN))
        .collect()
}

fn values_for(resp: &serde_json::Value, name: &str) -> Vec<f64> {
    field_for(resp, name, "values")
}

fn id_of(json: &serde_json::Value) -> String {
    json["id"].as_str().expect("created entity must have id").to_string()
}

/// Poll a reprocessing job until it completes (or fails / times out).
async fn poll_job(app: &axum::Router, token: &str, job_id: &str, max_secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(max_secs);
    loop {
        let (_s, job) =
            common::get_json_with_token(app, &format!("/api/reprocessing_jobs/{job_id}"), token).await;
        let status = job["status"].as_str().unwrap_or("");
        if status == "completed" || status == "failed" || Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

#[tokio::test]
#[serial]
async fn test_full_public_data_workflow() {
    let db = common::setup_test_db().await;
    common::cleanup_test_db(&db).await;
    let token = common::seed_api_token(&db, common::full_permissions(), None).await;
    let app = common::build_test_app(db.clone());

    // 1. Project (public).
    let (status, project) = common::post_json_parse_with_token(
        &app,
        "/api/projects",
        &serde_json::json!({
            "name": "E2E River",
            "description": "End-to-end workflow project",
            "is_public": true,
            "public_slug": "e2e_river",
            "public_api_title": "E2E River Public API",
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create project ({status}): {project}");
    let project_id = id_of(&project);

    // 2. Site.
    let (status, site) = common::post_json_parse_with_token(
        &app,
        "/api/sites",
        &serde_json::json!({
            "name": "E2E Station",
            "project_id": project_id,
            "latitude": 46.1,
            "longitude": 7.2,
            "public_slug": "e2e_station",
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create site ({status}): {site}");
    let site_id = id_of(&site);

    // 3. Raw parameters (global catalog).
    let (_s, do_param) = common::post_json_parse_with_token(
        &app,
        "/api/parameters",
        &serde_json::json!({
            "name": "dissolved_oxygen", "display_name": "Dissolved Oxygen",
            "default_units": "µM", "category": "measurement", "data_type": "numeric", "aliases": [],
        }),
        &token,
    )
    .await;
    let do_param_id = id_of(&do_param);
    let (_s, temp_param) = common::post_json_parse_with_token(
        &app,
        "/api/parameters",
        &serde_json::json!({
            "name": "temperature", "display_name": "Water Temperature",
            "default_units": "°C", "category": "measurement", "data_type": "numeric", "aliases": [],
        }),
        &token,
    )
    .await;
    let temp_param_id = id_of(&temp_param);

    // 4. Assign raw parameters to the site.
    for (pid, name) in [(&do_param_id, "DO"), (&temp_param_id, "Temp")] {
        let (status, sp) = common::post_json_with_token(
            &app,
            "/api/site_parameters",
            &serde_json::json!({
                "site_id": site_id, "parameter_id": pid, "name": name,
                "sensor_type": "measurement", "is_active": true,
            }),
            &token,
        )
        .await;
        assert!((200..300).contains(&status), "assign {name} ({status}): {sp}");
    }

    // 5. Sensor + identity calibration (part of station setup).
    let (_s, sensor) = common::post_json_parse_with_token(
        &app,
        "/api/sensors",
        &serde_json::json!({
            "parameter_id": do_param_id, "serial_number": "E2E-DO-1",
            "manufacturer": "Aanderaa", "model": "4531",
        }),
        &token,
    )
    .await;
    let sensor_id = id_of(&sensor);
    let (status, cal) = common::post_json_with_token(
        &app,
        "/api/sensor_calibrations",
        &serde_json::json!({
            "sensor_id": sensor_id, "slope": 1.0, "intercept": 0.0,
            "valid_from": "2025-01-01T00:00:00Z",
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create calibration ({status}): {cal}");

    // 6. Derived parameter DOmgL = dissolved_oxygen * 0.032 (after-create hook makes the output param).
    let (status, def) = common::post_json_parse_with_token(
        &app,
        "/api/derived_parameters",
        &serde_json::json!({
            "name": "DOmgL", "display_name": "Dissolved Oxygen mg/L",
            "units": "mg/L", "formula": "dissolved_oxygen * 0.032",
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create derived ({status}): {def}");
    let derived_def_id = id_of(&def);
    let derived_param_id = def["output_parameter_id"].as_str().unwrap().to_string();

    // 7. Assign the derived parameter to the site.
    let (status, sp) = common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": site_id, "parameter_id": derived_param_id, "name": "DOmgL",
            "sensor_type": "derived", "is_derived": true, "derived_definition_id": derived_def_id,
            "display_units": "mg/L",
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "assign derived ({status}): {sp}");

    // 8. Expose the two raw parameters plus the derived one publicly, under the client's names.
    for (pid, public_name, units, derived) in [
        (&do_param_id, "DOuM", "µM", false),
        (&temp_param_id, "WaterTempdegC", "°C", false),
        (&derived_param_id, "DOmgL", "mg/L", true),
    ] {
        let (status, pe) = common::post_json_with_token(
            &app,
            "/api/public_exposed_parameters",
            &serde_json::json!({
                "project_id": project_id, "parameter_id": pid,
                "public_name": public_name, "public_units": units,
                "conversion_factor": 1.0, "conversion_offset": 0.0, "include_derived": derived,
                "sort_order": 0,
            }),
            &token,
        )
        .await;
        assert!((200..300).contains(&status), "expose {public_name} ({status}): {pe}");
    }

    // 9. Ingest historical data via the CSV import endpoint (client wide-CSV format).
    let (status, body) = common::post_json_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": site_id, "csv": CSV }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import_csv ({status}): {body}");
    let imp: serde_json::Value = serde_json::from_str(&body).unwrap();
    let rows = parsed_csv();
    assert_eq!(
        imp["inserted_total"].as_u64().unwrap(),
        (rows.len() * 2) as u64,
        "{} rows × 2 raw params: {imp}",
        rows.len()
    );
    // Derived recompute + aggregate refresh run as a background reprocessing job; wait for it.
    let job_id = imp["derived_job_id"].as_str().expect("expected a derived job id");
    poll_job(&app, &token, job_id, 30).await;

    // 10. Public discovery + sites list reflect the new project/site.
    let (status, discovery) = common::get_json(&app, "/api/public").await;
    assert_eq!(status, 200);
    assert!(
        discovery.as_array().unwrap().iter().any(|p| p["slug"] == "e2e_river"),
        "discovery should list e2e_river: {discovery}"
    );
    let (status, sites) = common::get_json(&app, "/api/public/e2e_river/sites").await;
    assert_eq!(status, 200);
    assert!(sites.as_array().unwrap().iter().any(|s| s["uuid"] == site_id), "sites: {sites}");

    // 11. Public readings reproduce the real raw data exactly AND the recomputed derived value.
    let to_iso = |s: &str| s.replace(' ', "T") + "Z";
    let readings_uri = format!(
        "/api/public/e2e_river/sites/{site_id}/readings?start={}&end={}",
        to_iso(&rows.first().unwrap().0),
        to_iso(&rows.last().unwrap().0),
    );
    let (status, readings) = common::get_json(&app, &readings_uri).await;
    assert_eq!(status, 200, "public readings ({status}): {readings}");
    let got_doum = values_for(&readings, "DOuM");
    let got_temp = values_for(&readings, "WaterTempdegC");
    let got_domgl = values_for(&readings, "DOmgL");
    assert_eq!(got_doum.len(), rows.len(), "expected {} readings, got {}", rows.len(), got_doum.len());
    for (i, (_, doum, temp)) in rows.iter().enumerate() {
        assert!((got_doum[i] - doum).abs() < 1e-6, "DOuM[{i}]: {} != {doum}", got_doum[i]);
        assert!((got_temp[i] - temp).abs() < 1e-6, "WaterTempdegC[{i}]: {} != {temp}", got_temp[i]);
        assert!((got_domgl[i] - doum * 0.032).abs() < 1e-6, "DOmgL[{i}]: {} != {doum}*0.032", got_domgl[i]);
    }

    // 12. Hourly aggregate over the first hour equals the mean of that hour's real readings.
    let hour0: Vec<f64> = rows.iter().filter(|(dt, _, _)| &dt[11..13] == "00").map(|(_, d, _)| *d).collect();
    let expected_mean = hour0.iter().sum::<f64>() / hour0.len() as f64;
    let agg_uri = format!(
        "/api/public/e2e_river/sites/{site_id}/aggregates/hourly?start=2026-03-01T00:00:00Z&end=2026-03-01T00:59:00Z"
    );
    let (status, agg) = common::get_json(&app, &agg_uri).await;
    assert_eq!(status, 200, "public aggregates ({status}): {agg}");
    let do_avg = field_for(&agg, "DOuM", "avg");
    assert!(
        do_avg.first().is_some_and(|v| (v - expected_mean).abs() < 1e-6),
        "hourly DOuM mean should be {expected_mean}, got {do_avg:?}"
    );
    let domgl_avg = field_for(&agg, "DOmgL", "avg");
    assert!(
        domgl_avg.first().is_some_and(|v| (v - expected_mean * 0.032).abs() < 1e-6),
        "hourly DOmgL mean should be {} (mean × 0.032), got {domgl_avg:?}",
        expected_mean * 0.032
    );
}
