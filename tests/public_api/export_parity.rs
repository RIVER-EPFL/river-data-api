//! Export and contract parity across the readings, aggregate and alarm surfaces.
//!
//! The governing property: one logical query returns the same rows, the same values, the same
//! precision and the same null representation whether it is asked for as JSON, CSV or NDJSON, and
//! a site reports the same definition of its own data to every consumer.
//!
//! Each test names the defect id it proves; the ids are documented in `docs/defect-findings.md`.
//! These run as a real Keycloak user so the flows are the ones a person performs, and self-skip
//! when Keycloak is unreachable unless `REQUIRE_KEYCLOAK` is set.
//!
//! Run: cargo test --test public_api export_parity -- --test-threads=1

use axum::Router;
use axum::body::Body;
use axum::http::HeaderMap;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use serial_test::serial;
use tower::ServiceExt;

use crate::common::e2e;
use crate::common::keycloak as kc;

// --- Provisioning through the real HTTP surface ---

async fn register_stream(app: &Router, jwt: &str, source_key: &str) -> String {
    let (status, stream) = crate::common::post_json_parse_with_token(
        app,
        "/api/streams/register",
        &json!({
            "source_system": "export_parity",
            "source_key": source_key,
            "source_name": source_key,
        }),
        jwt,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "register {source_key} ({status}): {stream}"
    );
    e2e::id_of(&stream)
}

async fn pair_stream(app: &Router, jwt: &str, stream_id: &str, sp_id: &str) {
    let (status, paired) = crate::common::post_json_with_token(
        app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({ "site_parameter_id": sp_id }),
        jwt,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "pair {stream_id} ({status}): {paired}"
    );
}

/// Register a stream and pair it to a site_parameter, the sync-service onboarding order.
async fn open_stream(app: &Router, jwt: &str, source_key: &str, sp_id: &str) -> String {
    let stream_id = register_stream(app, jwt, source_key).await;
    pair_stream(app, jwt, &stream_id, sp_id).await;
    stream_id
}

/// Same, with the stream bound to an existing sensor first. Pairing an unbound stream mints a
/// fresh serial-less sensor for the feed, and the readings would be attributed to that one.
async fn open_stream_for_sensor(
    app: &Router,
    jwt: &str,
    source_key: &str,
    sp_id: &str,
    sensor_id: &str,
) -> String {
    let stream_id = register_stream(app, jwt, source_key).await;
    e2e::link_stream_sensor(app, jwt, &stream_id, sensor_id).await;
    pair_stream(app, jwt, &stream_id, sp_id).await;
    stream_id
}

async fn ingest(app: &Router, jwt: &str, stream_id: &str, points: &[(&str, f64)]) {
    let readings: Vec<Value> = points
        .iter()
        .map(|(time, value)| json!({ "time": time, "raw_value": value }))
        .collect();
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/ingest",
        &json!({ "stream_id": stream_id, "readings": readings }),
        jwt,
    )
    .await;
    assert_eq!(status, 200, "ingest into {stream_id} ({status}): {body}");
    assert_eq!(
        body["inserted"].as_u64(),
        Some(points.len() as u64),
        "every ingested point lands: {body}"
    );
    assert_eq!(
        body["paired"], true,
        "the stream is paired before ingest: {body}"
    );
}

async fn create_threshold(
    app: &Router,
    jwt: &str,
    site_id: &str,
    parameter_id: &str,
    warning_max: f64,
    alarm_max: f64,
) {
    let (status, body) = crate::common::post_json_with_token(
        app,
        "/api/alarm_thresholds",
        &json!({
            "parameter_id": parameter_id,
            "site_id": site_id,
            "warning_min": -1000.0,
            "warning_max": warning_max,
            "alarm_min": -10000.0,
            "alarm_max": alarm_max,
        }),
        jwt,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create threshold ({status}): {body}"
    );
}

// --- Response readers ---

/// Unauthenticated GET keeping the response headers, for the public tier.
async fn get_with_headers(app: &Router, uri: &str) -> (u16, HeaderMap, String) {
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).to_string())
}

fn content_type(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// One parameter block out of a readings/aggregates/alarms response, matched on any of the ids or
/// codes the various surfaces key by.
fn param_block<'a>(resp: &'a Value, key: &str) -> &'a Value {
    let params = resp["parameters"]
        .as_array()
        .unwrap_or_else(|| panic!("no 'parameters' array in {resp}"));
    params
        .iter()
        .find(|p| p["code"] == key || p["parameter_id"] == key || p["id"] == key)
        .unwrap_or_else(|| panic!("parameter {key} missing in {resp}"))
}

/// A parameter's value array with nulls preserved, which `e2e::values_for` collapses to NaN.
fn optional_values(resp: &Value, key: &str) -> Vec<Option<f64>> {
    param_block(resp, key)["values"]
        .as_array()
        .unwrap_or_else(|| panic!("no 'values' array for {key} in {resp}"))
        .iter()
        .map(serde_json::Value::as_f64)
        .collect()
}

fn times_of(resp: &Value) -> Vec<String> {
    resp["times"]
        .as_array()
        .unwrap_or_else(|| panic!("no 'times' array in {resp}"))
        .iter()
        .map(|t| t.as_str().unwrap_or_default().to_string())
        .collect()
}

fn csv_rows(body: &str) -> Vec<Vec<String>> {
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split(',').map(str::to_string).collect())
        .collect()
}

fn column_index(header: &[String], name: &str) -> usize {
    header
        .iter()
        .position(|c| c == name)
        .unwrap_or_else(|| panic!("column {name} missing from header {header:?}"))
}

fn ndjson_objects(body: &str) -> Vec<Value> {
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| panic!("NDJSON line is not JSON: {e}\n{l}"))
        })
        .collect()
}

/// a flagged reading must be excluded from every surface that claims to serve the site's
/// data, so the public count, the public series and the aggregate agree on one definition.
#[tokio::test]
#[serial]
async fn flagged_readings_are_served_consistently_by_count_series_and_aggregate() {
    if !kc::require_keycloak_or_skip("flagged_readings_consistency").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let project = e2e::create_project(&app, &jwt, "RD045 Project", "rd045", true).await;
    let site = e2e::create_site(&app, &jwt, &project, "RD045 Station", "rd045-station").await;
    let parameter = e2e::create_parameter(&app, &jwt, "Rd045Depth", "RD045 Depth", "mm").await;
    let sp = e2e::assign_site_parameter_minimal(&app, &jwt, &site, &parameter).await;
    e2e::set_site_parameter_public(&db, &sp).await;

    let stream = open_stream(&app, &jwt, "rd045-depth", &sp).await;
    let flagged_time = "2025-06-04T04:00:00Z";
    ingest(
        &app,
        &jwt,
        &stream,
        &[
            ("2025-06-04T00:00:00Z", 100.0),
            ("2025-06-04T01:00:00Z", 200.0),
            ("2025-06-04T02:00:00Z", 300.0),
            ("2025-06-04T03:00:00Z", 400.0),
            (flagged_time, 500.0),
        ],
    )
    .await;

    let (status, flagged) = crate::common::patch_json_with_token(
        &app,
        "/api/readings/flag",
        &json!({
            "readings": [{ "site_id": site, "parameter_id": parameter, "time": flagged_time }],
            "reason": "instrument out of water",
        }),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "flag the outlier ({status}): {flagged}");
    let flagged: Value = serde_json::from_str(&flagged).unwrap();
    assert_eq!(
        flagged["updated"].as_u64(),
        Some(1),
        "exactly one reading flagged: {flagged}"
    );

    let (status, detail) =
        crate::common::get_json(&app, "/api/public/rd045/sites/rd045-station").await;
    assert_eq!(status, 200, "public site detail ({status}): {detail}");
    assert_eq!(
        detail["reading_count"].as_i64(),
        Some(4),
        "the site counts the four unflagged readings: {detail}"
    );

    let window = "start=2025-06-04T00:00:00Z&end=2025-06-04T23:59:59Z";
    let (status, public) = crate::common::get_json(
        &app,
        &format!("/api/public/rd045/sites/rd045-station/readings?{window}"),
    )
    .await;
    assert_eq!(status, 200, "public readings ({status}): {public}");
    let public_values = optional_values(&public, "Rd045Depth");
    assert_eq!(
        public_values,
        vec![Some(100.0), Some(200.0), Some(300.0), Some(400.0)],
        "the public series serves the same four readings the site counts, \
         and the public payload has no field that could mark the flagged one: {public}"
    );
    assert_eq!(
        times_of(&public).len(),
        4,
        "the flagged timestamp is not on the public time axis: {public}"
    );

    let (status, private) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site}/readings?{window}"),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "private readings ({status}): {private}");
    let private_values = optional_values(&private, &parameter);
    assert_eq!(
        private_values,
        vec![
            Some(100.0),
            Some(200.0),
            Some(300.0),
            Some(400.0),
            Some(500.0)
        ],
        "the private default keeps the flagged reading, which it is entitled to do \
         because it labels it: {private}"
    );
    let flags = param_block(&private, &parameter)["flagged"].as_array();
    assert!(
        flags.is_some(),
        "the private series carries a per-point flag array: {private}"
    );
    let flags = flags.unwrap();
    assert_eq!(
        flags.iter().filter(|f| **f == Value::Bool(true)).count(),
        1,
        "exactly one private point is marked flagged: {private}"
    );
    assert_eq!(
        flags[4],
        Value::Bool(true),
        "and it is the 500 point: {private}"
    );

    let (status, unflagged) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site}/readings?{window}&include_flagged=false"),
        &jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "private readings without flagged ({status}): {unflagged}"
    );
    assert_eq!(
        optional_values(&unflagged, &parameter),
        vec![Some(100.0), Some(200.0), Some(300.0), Some(400.0)],
        "the private opt-out reproduces exactly what the public tier serves: {unflagged}"
    );

    let (status, aggregates) = crate::common::get_json(
        &app,
        &format!("/api/public/rd045/sites/rd045-station/aggregates/daily?{window}"),
    )
    .await;
    assert_eq!(
        status, 200,
        "public daily aggregates ({status}): {aggregates}"
    );
    let block = param_block(&aggregates, "Rd045Depth");
    assert_eq!(
        block["count"][0].as_i64(),
        Some(4),
        "the daily bucket rolls up four readings: {aggregates}"
    );
    assert_eq!(
        block["avg"][0].as_f64(),
        Some(250.0),
        "and its mean is the mean of the four unflagged values: {aggregates}"
    );
}

/// `alarms` and `include_sample_stats` are accepted on CSV and NDJSON exports, so the data
/// they ask for must appear there, not only in JSON.
#[tokio::test]
#[serial]
async fn csv_and_ndjson_exports_honour_the_readings_opt_ins() {
    if !kc::require_keycloak_or_skip("readings_export_opt_ins").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let project = e2e::create_project(&app, &jwt, "RD046 Project", "rd046", false).await;
    let site = e2e::create_site(&app, &jwt, &project, "RD046 Station", "rd046-station").await;
    let parameter = e2e::create_parameter(&app, &jwt, "Rd046Turb", "RD046 Turbidity", "NTU").await;
    let sp = e2e::assign_site_parameter_minimal(&app, &jwt, &site, &parameter).await;
    create_threshold(&app, &jwt, &site, &parameter, 100.0, 1000.0).await;

    let stream = open_stream(&app, &jwt, "rd046-turb", &sp).await;
    let breach_time = "2025-06-05T01:00:00Z";
    ingest(
        &app,
        &jwt,
        &stream,
        &[("2025-06-05T00:00:00Z", 10.0), (breach_time, 500.0)],
    )
    .await;

    let grab_time = "2025-06-05T02:00:00Z";
    let (status, grab) = crate::common::post_json_parse_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": site,
            "label": "rd046",
            "readings": crate::common::tracks::grab_replicates(
                &parameter,
                grab_time,
                &[20.0, 30.0, 40.0],
            ),
        }),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "grab sample ({status}): {grab}");
    assert_eq!(
        grab["samples_created"], 1,
        "one replicate group recorded: {grab}"
    );

    let window = "start=2025-06-05T00:00:00Z&end=2025-06-05T03:00:00Z";
    let base = format!("/api/sites/{site}/readings?{window}");

    let (status, with_alarms) =
        crate::common::get_json_with_token(&app, &format!("{base}&alarms=true"), &jwt).await;
    assert_eq!(status, 200, "JSON with alarms ({status}): {with_alarms}");
    let severities = param_block(&with_alarms, &parameter)["severities"].as_array();
    assert!(
        severities.is_some(),
        "JSON carries the opt-in severities: {with_alarms}"
    );
    let severities = severities.unwrap();
    let breach_index = times_of(&with_alarms)
        .iter()
        .position(|t| t.starts_with("2025-06-05T01:00:00"));
    assert!(
        breach_index.is_some(),
        "the breaching point is on the axis: {with_alarms}"
    );
    let breach_index = breach_index.unwrap();
    assert_eq!(
        severities[breach_index].as_i64(),
        Some(1),
        "500 breaches the warning bound of 100: {with_alarms}"
    );

    let (status, csv) =
        crate::common::get_with_token(&app, &format!("{base}&alarms=true&format=csv"), &jwt).await;
    assert_eq!(status, 200, "CSV with alarms ({status}): {csv}");
    let rows = csv_rows(&csv);
    let header = rows.first().cloned().unwrap_or_default();
    assert!(
        header.iter().any(|c| c == "Rd046Turb_severity"),
        "the CSV export carries the severity the caller opted into: {header:?}"
    );
    let severity_col = column_index(&header, "Rd046Turb_severity");
    let breach_row = rows
        .iter()
        .skip(1)
        .find(|r| r[0].starts_with("2025-06-05T01:00:00"));
    assert!(breach_row.is_some(), "the breaching row is exported: {csv}");
    assert_eq!(
        breach_row.unwrap()[severity_col],
        "1",
        "and it carries the same severity JSON reports: {csv}"
    );

    let (status, ndjson) =
        crate::common::get_with_token(&app, &format!("{base}&alarms=true&format=ndjson"), &jwt)
            .await;
    assert_eq!(status, 200, "NDJSON with alarms ({status}): {ndjson}");
    let objects = ndjson_objects(&ndjson);
    let breach_obj = objects.iter().find(|o| {
        o["time"]
            .as_str()
            .unwrap_or_default()
            .starts_with("2025-06-05T01:00:00")
    });
    assert!(
        breach_obj.is_some(),
        "the breaching object is exported: {ndjson}"
    );
    assert_eq!(
        breach_obj.unwrap()["Rd046Turb_severity"].as_i64(),
        Some(1),
        "NDJSON carries the opt-in severity too: {ndjson}"
    );

    let (status, with_stats) = crate::common::get_json_with_token(
        &app,
        &format!("{base}&include_sample_stats=true"),
        &jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "JSON with sample stats ({status}): {with_stats}"
    );
    let samples = param_block(&with_stats, &parameter)["samples"].as_array();
    assert!(
        samples.is_some(),
        "JSON carries the opt-in sample stats: {with_stats}"
    );
    let grab_index = times_of(&with_stats)
        .iter()
        .position(|t| t.starts_with("2025-06-05T02:00:00"));
    assert!(
        grab_index.is_some(),
        "the grab point is on the axis: {with_stats}"
    );
    assert_eq!(
        samples.unwrap()[grab_index.unwrap()]["mean"].as_f64(),
        Some(30.0),
        "the mean of 20/30/40 is 30: {with_stats}"
    );

    let (status, plain_csv) =
        crate::common::get_with_token(&app, &format!("{base}&format=csv"), &jwt).await;
    assert_eq!(status, 200, "plain CSV ({status}): {plain_csv}");
    let stats_uri = format!("{base}&include_sample_stats=true&format=csv");
    let (status, stats_csv) = crate::common::get_with_token(&app, &stats_uri, &jwt).await;
    assert_eq!(status, 200, "CSV with sample stats ({status}): {stats_csv}");
    let plain_header = csv_rows(&plain_csv).first().cloned().unwrap_or_default();
    let stats_header = csv_rows(&stats_csv).first().cloned().unwrap_or_default();
    assert_ne!(
        stats_header, plain_header,
        "opting into sample stats must change the CSV export, not be accepted and dropped: \
         {stats_header:?}"
    );
    assert_eq!(
        plain_header,
        vec!["time".to_string(), "Rd046Turb".to_string()],
        "the export without opt-ins is time plus one column per parameter code: {plain_header:?}"
    );
}

/// a sensor bound to two parameters must serve one parameter's series, not both merged
/// into the single array the response labels with one parameter id.
#[tokio::test]
#[serial]
async fn sensor_readings_serve_one_parameter_not_every_channel() {
    if !kc::require_keycloak_or_skip("sensor_readings_single_parameter").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let project = e2e::create_project(&app, &jwt, "RD047 Project", "rd047", false).await;
    let site = e2e::create_site(&app, &jwt, &project, "RD047 Station", "rd047-station").await;
    let cond = e2e::create_parameter(&app, &jwt, "Rd047Cond", "RD047 Conductivity", "uS/cm").await;
    let temp = e2e::create_parameter(&app, &jwt, "Rd047Temp", "RD047 Temperature", "degC").await;
    let sp_cond = e2e::assign_site_parameter_minimal(&app, &jwt, &site, &cond).await;
    let sp_temp = e2e::assign_site_parameter_minimal(&app, &jwt, &site, &temp).await;

    let sensor = e2e::create_sensor(&app, &jwt, &cond, "RD047-0001").await;
    e2e::create_deployment(&app, &jwt, &sensor, &site, &cond, "2025-06-01T00:00:00Z").await;
    e2e::create_deployment(&app, &jwt, &sensor, &site, &temp, "2025-06-01T01:00:00Z").await;

    let cond_stream = open_stream_for_sensor(&app, &jwt, "rd047-cond", &sp_cond, &sensor).await;
    let temp_stream = open_stream_for_sensor(&app, &jwt, "rd047-temp", &sp_temp, &sensor).await;
    ingest(
        &app,
        &jwt,
        &cond_stream,
        &[
            ("2025-06-02T00:00:00Z", 500.0),
            ("2025-06-02T01:00:00Z", 501.0),
            ("2025-06-02T02:00:00Z", 502.0),
        ],
    )
    .await;
    ingest(
        &app,
        &jwt,
        &temp_stream,
        &[
            ("2025-06-02T03:00:00Z", 10.0),
            ("2025-06-02T04:00:00Z", 11.0),
            ("2025-06-02T05:00:00Z", 12.0),
        ],
    )
    .await;

    let window = "start=2025-06-02T00:00:00Z&end=2025-06-02T23:59:59Z";
    let (status, raw) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sensors/{sensor}/readings?{window}"),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "sensor readings ({status}): {raw}");

    let reported = raw["parameter_id"].as_str();
    assert!(
        reported.is_some(),
        "the response names the parameter it serves: {raw}"
    );
    let reported = reported.unwrap();
    let (expected_series, expected_mean, expected_units) = if reported == cond {
        (vec![500.0, 501.0, 502.0], 501.0, "uS/cm")
    } else if reported == temp {
        (vec![10.0, 11.0, 12.0], 11.0, "degC")
    } else {
        panic!("the reported parameter belongs to neither channel of this sensor: {raw}");
    };

    assert_eq!(
        raw["units"].as_str(),
        Some(expected_units),
        "the units belong to the parameter the response names: {raw}"
    );
    let served: Vec<f64> = raw["raw"]
        .as_array()
        .unwrap_or_else(|| panic!("no 'raw' array in {raw}"))
        .iter()
        .map(|v| v.as_f64().unwrap_or(f64::NAN))
        .collect();
    assert_eq!(
        served, expected_series,
        "the series holds only the named parameter's readings; the other channel's quantity \
         must not share the array: {raw}"
    );

    let (status, daily) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sensors/{sensor}/readings?{window}&resolution=daily"),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "sensor daily readings ({status}): {daily}");
    assert_eq!(
        daily["parameter_id"].as_str(),
        Some(reported),
        "the bucketed arm names the same parameter as the raw arm: {daily}"
    );
    let buckets = daily["raw"]
        .as_array()
        .unwrap_or_else(|| panic!("no 'raw' array in {daily}"));
    assert_eq!(
        buckets.len(),
        1,
        "all six readings fall on one day: {daily}"
    );
    assert_eq!(
        buckets[0].as_f64(),
        Some(expected_mean),
        "the bucket averages one physical quantity, not two: {daily}"
    );
}

/// public aggregates must list the requested site's exposed parameters, not the whole
/// project's, so no phantom all-null series appears.
#[tokio::test]
#[serial]
async fn public_aggregates_list_only_the_requested_sites_parameters() {
    if !kc::require_keycloak_or_skip("public_aggregate_phantom_series").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let project = e2e::create_project(&app, &jwt, "RD053 Project", "rd053", true).await;
    let wide = e2e::create_site(&app, &jwt, &project, "RD053 Wide", "rd053-wide").await;
    let narrow = e2e::create_site(&app, &jwt, &project, "RD053 Narrow", "rd053-narrow").await;
    let shared = e2e::create_parameter(&app, &jwt, "Rd053Depth", "RD053 Depth", "mm").await;
    let extra = e2e::create_parameter(&app, &jwt, "Rd053Cdom", "RD053 CDOM", "ppb").await;

    let sp_wide_shared = e2e::assign_site_parameter_minimal(&app, &jwt, &wide, &shared).await;
    let sp_wide_extra = e2e::assign_site_parameter_minimal(&app, &jwt, &wide, &extra).await;
    let sp_narrow_shared = e2e::assign_site_parameter_minimal(&app, &jwt, &narrow, &shared).await;
    for sp in [&sp_wide_shared, &sp_wide_extra, &sp_narrow_shared] {
        e2e::set_site_parameter_public(&db, sp).await;
    }

    let wide_shared_stream = open_stream(&app, &jwt, "rd053-wide-depth", &sp_wide_shared).await;
    let wide_extra_stream = open_stream(&app, &jwt, "rd053-wide-cdom", &sp_wide_extra).await;
    let narrow_stream = open_stream(&app, &jwt, "rd053-narrow-depth", &sp_narrow_shared).await;
    ingest(
        &app,
        &jwt,
        &wide_shared_stream,
        &[
            ("2025-06-06T00:00:00Z", 9.0),
            ("2025-06-06T01:00:00Z", 11.0),
        ],
    )
    .await;
    ingest(
        &app,
        &jwt,
        &wide_extra_stream,
        &[
            ("2025-06-06T00:00:00Z", 19.0),
            ("2025-06-06T01:00:00Z", 21.0),
        ],
    )
    .await;
    ingest(
        &app,
        &jwt,
        &narrow_stream,
        &[
            ("2025-06-06T00:00:00Z", 29.0),
            ("2025-06-06T01:00:00Z", 31.0),
        ],
    )
    .await;

    let (status, refresh) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/refresh_aggregates",
        &json!({ "full": true }),
        &jwt,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "refresh aggregates ({status}): {refresh}"
    );
    let job_id = refresh["job_id"].as_str();
    assert!(job_id.is_some(), "the refresh is a tracked job: {refresh}");
    assert_eq!(
        e2e::poll_job(&app, &jwt, job_id.unwrap(), 30).await,
        "completed",
        "the aggregate refresh completes"
    );

    let window = "start=2025-06-06T00:00:00Z&end=2025-06-06T23:59:59Z";
    let (status, narrow_agg) = crate::common::get_json(
        &app,
        &format!("/api/public/rd053/sites/rd053-narrow/aggregates/daily?{window}"),
    )
    .await;
    assert_eq!(
        status, 200,
        "narrow site aggregates ({status}): {narrow_agg}"
    );
    let narrow_codes: Vec<String> = narrow_agg["parameters"]
        .as_array()
        .unwrap_or_else(|| panic!("no 'parameters' array in {narrow_agg}"))
        .iter()
        .map(|p| p["code"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        narrow_codes,
        vec!["Rd053Depth".to_string()],
        "a site exposes only its own parameters, not the project's: {narrow_agg}"
    );

    let (status, narrow_readings) = crate::common::get_json(
        &app,
        &format!("/api/public/rd053/sites/rd053-narrow/readings?{window}"),
    )
    .await;
    assert_eq!(
        status, 200,
        "narrow site readings ({status}): {narrow_readings}"
    );
    let readings_codes: Vec<String> = narrow_readings["parameters"]
        .as_array()
        .unwrap_or_else(|| panic!("no 'parameters' array in {narrow_readings}"))
        .iter()
        .map(|p| p["code"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        narrow_codes, readings_codes,
        "the two public series endpoints agree on which parameters the site has"
    );

    let narrow_block = param_block(&narrow_agg, "Rd053Depth");
    assert_eq!(
        narrow_block["avg"][0].as_f64(),
        Some(30.0),
        "the site's own series is unaffected: {narrow_agg}"
    );
    assert_eq!(
        narrow_block["count"][0].as_i64(),
        Some(2),
        "both of its readings roll up: {narrow_agg}"
    );

    let (status, wide_agg) = crate::common::get_json(
        &app,
        &format!("/api/public/rd053/sites/rd053-wide/aggregates/daily?{window}"),
    )
    .await;
    assert_eq!(status, 200, "wide site aggregates ({status}): {wide_agg}");
    let wide_codes: Vec<String> = wide_agg["parameters"]
        .as_array()
        .unwrap_or_else(|| panic!("no 'parameters' array in {wide_agg}"))
        .iter()
        .map(|p| p["code"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        wide_codes,
        vec!["Rd053Cdom".to_string(), "Rd053Depth".to_string()],
        "the site that does expose both keeps both: {wide_agg}"
    );
    assert_eq!(
        param_block(&wide_agg, "Rd053Cdom")["avg"][0].as_f64(),
        Some(20.0),
        "and both carry their own values: {wide_agg}"
    );
    assert_eq!(
        param_block(&wide_agg, "Rd053Depth")["avg"][0].as_f64(),
        Some(10.0),
        "and both carry their own values: {wide_agg}"
    );
}

/// `/sites/{id}/alarms` must report null where a parameter did not violate, matching every
/// other series endpoint, rather than a literal 0.0 on the shared time axis.
#[tokio::test]
#[serial]
async fn site_alarms_leave_non_violating_timestamps_null_in_every_format() {
    if !kc::require_keycloak_or_skip("site_alarms_null_fill").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let project = e2e::create_project(&app, &jwt, "RD054 Project", "rd054", false).await;
    let site = e2e::create_site(&app, &jwt, &project, "RD054 Station", "rd054-station").await;
    let alpha = e2e::create_parameter(&app, &jwt, "Rd054Alpha", "RD054 Alpha", "NTU").await;
    let beta = e2e::create_parameter(&app, &jwt, "Rd054Beta", "RD054 Beta", "NTU").await;
    let sp_alpha = e2e::assign_site_parameter_minimal(&app, &jwt, &site, &alpha).await;
    let sp_beta = e2e::assign_site_parameter_minimal(&app, &jwt, &site, &beta).await;
    create_threshold(&app, &jwt, &site, &alpha, 100.0, 1000.0).await;
    create_threshold(&app, &jwt, &site, &beta, 100.0, 1000.0).await;

    let alpha_stream = open_stream(&app, &jwt, "rd054-alpha", &sp_alpha).await;
    let beta_stream = open_stream(&app, &jwt, "rd054-beta", &sp_beta).await;
    ingest(
        &app,
        &jwt,
        &alpha_stream,
        &[("2025-06-07T00:00:00Z", 500.0)],
    )
    .await;
    ingest(&app, &jwt, &beta_stream, &[("2025-06-07T01:00:00Z", 600.0)]).await;

    let window = "start=2025-06-07T00:00:00Z&end=2025-06-07T23:59:59Z";
    let uri = format!("/api/sites/{site}/alarms?{window}");

    let (status, alarms) = crate::common::get_json_with_token(&app, &uri, &jwt).await;
    assert_eq!(status, 200, "site alarms ({status}): {alarms}");
    let times = times_of(&alarms);
    assert_eq!(
        times.len(),
        2,
        "the two breaches share one time axis: {alarms}"
    );

    let alpha_values = optional_values(&alarms, &alpha);
    let beta_values = optional_values(&alarms, &beta);
    assert_eq!(
        alpha_values[0],
        Some(500.0),
        "alpha's own breach is reported at its own timestamp: {alarms}"
    );
    assert_eq!(
        alpha_values[1], None,
        "alpha did not violate at beta's timestamp, so it reports no value there: {alarms}"
    );
    assert_eq!(
        beta_values[1],
        Some(600.0),
        "beta's own breach is reported at its own timestamp: {alarms}"
    );
    assert_eq!(
        beta_values[0], None,
        "and beta reports no value at alpha's timestamp: {alarms}"
    );
    let alpha_severities = param_block(&alarms, &alpha)["severities"].as_array();
    assert!(
        alpha_severities.is_some(),
        "severities accompany the values: {alarms}"
    );
    assert_eq!(
        alpha_severities.unwrap()[0].as_i64(),
        Some(1),
        "500 against a warning bound of 100 is a warning: {alarms}"
    );

    let (status, csv) =
        crate::common::get_with_token(&app, &format!("{uri}&format=csv"), &jwt).await;
    assert_eq!(status, 200, "site alarms CSV ({status}): {csv}");
    let rows = csv_rows(&csv);
    let header = rows.first().cloned().unwrap_or_default();
    let alpha_col = column_index(&header, "RD054 Alpha_value");
    let beta_col = column_index(&header, "RD054 Beta_value");
    assert_eq!(
        rows.len(),
        3,
        "a header and one row per violating timestamp: {csv}"
    );
    assert_eq!(
        rows[1][alpha_col], "500",
        "alpha's breach cell carries its value: {csv}"
    );
    assert_eq!(
        rows[2][alpha_col], "",
        "alpha's cell on beta's row is empty, not a fabricated 0: {csv}"
    );
    assert_eq!(
        rows[2][beta_col], "600",
        "beta's breach cell carries its value: {csv}"
    );
    assert_eq!(
        rows[1][beta_col], "",
        "beta's cell on alpha's row is empty, not a fabricated 0: {csv}"
    );

    let (status, ndjson) =
        crate::common::get_with_token(&app, &format!("{uri}&format=ndjson"), &jwt).await;
    assert_eq!(status, 200, "site alarms NDJSON ({status}): {ndjson}");
    let objects = ndjson_objects(&ndjson);
    assert_eq!(
        objects.len(),
        2,
        "one object per violating timestamp: {ndjson}"
    );
    assert_eq!(
        objects[0]["RD054 Alpha_value"].as_f64(),
        Some(500.0),
        "alpha's breach is on its own object: {ndjson}"
    );
    let alpha_on_beta_row = objects[1].get("RD054 Alpha_value");
    assert!(
        alpha_on_beta_row.is_none_or(Value::is_null),
        "alpha carries no value on beta's object: {ndjson}"
    );
    assert_eq!(
        objects[1]["RD054 Beta_value"].as_f64(),
        Some(600.0),
        "beta's breach is on its own object: {ndjson}"
    );
}

/// an empty result must still be delivered in the format the caller asked for, on the
/// private readings, aggregates and alarms handlers and on the public aggregates handler.
#[tokio::test]
#[serial]
async fn empty_results_are_delivered_in_the_requested_format() {
    if !kc::require_keycloak_or_skip("empty_result_format").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let project = e2e::create_project(&app, &jwt, "RD056 Project", "rd056", true).await;
    let stocked = e2e::create_site(&app, &jwt, &project, "RD056 Stocked", "rd056-stocked").await;
    let bare = e2e::create_site(&app, &jwt, &project, "RD056 Bare", "rd056-bare").await;
    let parameter = e2e::create_parameter(&app, &jwt, "Rd056Depth", "RD056 Depth", "mm").await;
    let sp = e2e::assign_site_parameter_minimal(&app, &jwt, &stocked, &parameter).await;
    e2e::set_site_parameter_public(&db, &sp).await;
    create_threshold(&app, &jwt, &stocked, &parameter, 100.0, 1000.0).await;

    let stream = open_stream(&app, &jwt, "rd056-depth", &sp).await;
    ingest(
        &app,
        &jwt,
        &stream,
        &[
            ("2025-06-08T00:00:00Z", 10.0),
            ("2025-06-08T00:10:00Z", 12.0),
            ("2025-06-08T02:00:00Z", 500.0),
        ],
    )
    .await;

    let quiet = "start=2025-06-08T00:00:00Z&end=2025-06-08T00:30:00Z";
    let full = "start=2025-06-08T00:00:00Z&end=2025-06-08T23:59:59Z";

    let (status, headers, body) = crate::common::get_with_token_headers(
        &app,
        &format!("/api/sites/{stocked}/alarms?{quiet}&format=csv"),
        &jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "alarms CSV over a quiet window ({status}): {body}"
    );
    assert!(
        content_type(&headers).starts_with("text/csv"),
        "a window with no breaches still answers as CSV, got {} with body: {body}",
        content_type(&headers)
    );
    assert!(
        !body.trim_start().starts_with('{'),
        "and its body is not a JSON document: {body}"
    );

    let (status, headers, body) = crate::common::get_with_token_headers(
        &app,
        &format!("/api/sites/{stocked}/alarms?{full}&format=csv"),
        &jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "alarms CSV over a breaching window ({status}): {body}"
    );
    assert!(
        content_type(&headers).starts_with("text/csv"),
        "the non-empty case is CSV, got {}: {body}",
        content_type(&headers)
    );

    let (status, headers, body) = crate::common::get_with_token_headers(
        &app,
        &format!("/api/sites/{bare}/readings?{full}&format=csv"),
        &jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "readings CSV for a site with no parameters ({status}): {body}"
    );
    assert!(
        content_type(&headers).starts_with("text/csv"),
        "a site with no parameters still answers as CSV, got {} with body: {body}",
        content_type(&headers)
    );

    let (status, headers, body) = crate::common::get_with_token_headers(
        &app,
        &format!("/api/sites/{bare}/readings?{full}&format=ndjson"),
        &jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "readings NDJSON for a site with no parameters ({status}): {body}"
    );
    assert!(
        content_type(&headers).starts_with("application/x-ndjson"),
        "and as NDJSON when that is what was asked for, got {} with body: {body}",
        content_type(&headers)
    );

    let (status, headers, body) = crate::common::get_with_token_headers(
        &app,
        &format!("/api/sites/{bare}/aggregates/daily?{full}&format=csv"),
        &jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "aggregates CSV for a site with no parameters ({status}): {body}"
    );
    assert!(
        content_type(&headers).starts_with("text/csv"),
        "the aggregates handler honours the format on the empty path too, got {} with body: {body}",
        content_type(&headers)
    );

    let (status, headers, body) = crate::common::get_with_token_headers(
        &app,
        &format!("/api/sites/{stocked}/readings?{full}&format=csv"),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "readings CSV with data ({status}): {body}");
    assert!(
        content_type(&headers).starts_with("text/csv"),
        "the populated case is CSV, got {}: {body}",
        content_type(&headers)
    );

    let (status, headers, body) = get_with_headers(
        &app,
        &format!("/api/public/rd056/sites/rd056-bare/aggregates/daily?{full}&format=csv"),
    )
    .await;
    assert_eq!(
        status, 200,
        "public aggregates CSV, nothing exposed ({status}): {body}"
    );
    assert!(
        content_type(&headers).starts_with("text/csv"),
        "the public tier honours the format when the site exposes nothing, got {}: {body}",
        content_type(&headers)
    );

    let (status, headers, body) = get_with_headers(
        &app,
        &format!("/api/public/rd056/sites/rd056-stocked/aggregates/daily?{full}&format=csv"),
    )
    .await;
    assert_eq!(
        status, 200,
        "public aggregates CSV for the exposed site ({status}): {body}"
    );
    assert!(
        content_type(&headers).starts_with("text/csv"),
        "the exposed site's public export is CSV, got {}: {body}",
        content_type(&headers)
    );
}
