//! Track C: grab entry through the analytical tools, and the sensor-vs-grab comparison export.
//!
//! Scenario: a lab technician runs a tool over a real portal replicate triplicate, saves the three
//! replicates at a station through a real standard curve, and an analyst then compares those grabs
//! against the continuous series at the same slot.
//! Expected behaviour: the tool's own average and standard deviation survive the round trip through
//! the samples trigger and come back as one served spot point, and the comparison export pairs each
//! grab with the continuous readings in `[T + window_start_hours, T + window_end_hours]`, inclusive
//! at both ends, excluding grab replicates from the sensor side however wide the window is.
//!
//! Every step runs as the lowest role that should be able to perform it: tools and the export are
//! `read_data` (intern), grab entry and batch ingest are `write_data` (river), provisioning is
//! Administrator. The role below is asserted refused where the flow crosses a gate.

use axum::Router;
use serde_json::json;
use serial_test::serial;

use crate::common::e2e;
use crate::common::keycloak as kc;
use crate::common::tracks::{self, BAND_GRAB, Track};

const PORTAL_GRAB_ROWS: &str = include_str!("../fixtures/portal_grab_rows_metalp.csv");
const PORTAL_CURVES: &str = include_str!("../fixtures/portal_standard_curves_metalp.csv");

/// The day the synthetic sensor-vs-grab fixtures sit on. Past-dated on purpose: any window the
/// export resolves is bounded by the data, and past timestamps also satisfy the batch endpoint's
/// admission window (an absolute floor, one day of lead).
const DAY: &str = "2025-06-05";

// ---------------------------------------------------------------------------------------------
// Fixture access
// ---------------------------------------------------------------------------------------------

fn csv_rows(csv: &str) -> Vec<std::collections::HashMap<&str, &str>> {
    let mut lines = csv.lines().filter(|l| !l.trim().is_empty());
    let header: Vec<&str> = lines
        .next()
        .expect("fixture has a header row")
        .split(',')
        .collect();
    lines
        .map(|line| header.iter().copied().zip(line.split(',')).collect())
        .collect()
}

/// A complete DOC triplicate from the portal dump, alongside the average and standard deviation
/// the portal itself recorded for it.
struct PortalDoc {
    replicates: Vec<f64>,
    recorded_avg: f64,
    recorded_sd: f64,
}

fn portal_doc_triplicate(station: &str, date: &str) -> PortalDoc {
    let rows = csv_rows(PORTAL_GRAB_ROWS);
    let row = rows
        .iter()
        .find(|r| {
            r.get("station").copied() == Some(station)
                && r.get("DATE_reading").copied() == Some(date)
        })
        .unwrap_or_else(|| panic!("portal fixture holds no {station} row on {date}"));
    let num = |key: &str| -> f64 {
        row.get(key)
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("{key} is empty on the {station} {date} row"))
    };
    PortalDoc {
        replicates: vec![num("DOC_rep_1"), num("DOC_rep_2"), num("DOC_rep_3")],
        recorded_avg: num("DOC_avg_ppb"),
        recorded_sd: num("DOC_sd_ppb"),
    }
}

/// The (slope, intercept) the portal recorded for a named standard curve.
fn portal_curve(parameter: &str) -> (f64, f64) {
    let rows = csv_rows(PORTAL_CURVES);
    let row = rows
        .iter()
        .find(|r| r.get("parameter").copied() == Some(parameter))
        .unwrap_or_else(|| panic!("portal fixture holds no standard curve for {parameter}"));
    let num = |key: &str| -> f64 {
        row[key].parse::<f64>().unwrap_or_else(|_| {
            panic!("standard curve column {key} is not numeric for {parameter}")
        })
    };
    (num("a"), num("b"))
}

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

/// A Keycloak fixture user at `role`, granted visibility of the track's project. Fixture passwords
/// equal the username.
async fn member(
    db: &sea_orm::DatabaseConnection,
    project_id: &str,
    user: &str,
    role: &str,
) -> String {
    kc::ensure_realm_user(user, user, &[role]).await;
    kc::grant_project(db, &kc::keycloak_user_id(user).await, project_id).await;
    kc::get_keycloak_jwt(user, user).await
}

/// Add a parameter to the track's site over HTTP, for slots the track itself does not provision.
async fn add_parameter(app: &Router, admin: &str, track: &Track, code: &str, name: &str) -> String {
    let parameter_id = e2e::create_parameter(app, admin, code, name, "ppb").await;
    e2e::assign_site_parameter_minimal(app, admin, &track.site_id, &parameter_id).await;
    parameter_id
}

fn parameter_block<'a>(resp: &'a serde_json::Value, parameter_id: &str) -> &'a serde_json::Value {
    resp["parameters"]
        .as_array()
        .unwrap_or_else(|| panic!("no 'parameters' array in response: {resp}"))
        .iter()
        .find(|p| p["parameter_id"] == parameter_id)
        .unwrap_or_else(|| panic!("parameter {parameter_id} missing in {resp}"))
}

fn f64_at(value: &serde_json::Value, key: &str) -> f64 {
    value[key]
        .as_f64()
        .unwrap_or_else(|| panic!("'{key}' is not a number in {value}"))
}

/// A row's `time` as an instant, so the assertion is about the moment rather than its formatting.
fn time_at(row: &serde_json::Value) -> chrono::DateTime<chrono::Utc> {
    let raw = row["time"]
        .as_str()
        .unwrap_or_else(|| panic!("row carries a time: {row}"));
    chrono::DateTime::parse_from_rfc3339(raw)
        .unwrap_or_else(|e| panic!("time {raw} is not RFC 3339 ({e}): {row}"))
        .with_timezone(&chrono::Utc)
}

fn instant(hhmm: &str) -> chrono::DateTime<chrono::Utc> {
    let raw = format!("{DAY}T{hhmm}:00Z");
    chrono::DateTime::parse_from_rfc3339(&raw)
        .unwrap_or_else(|e| panic!("fixture time {raw} is not RFC 3339: {e}"))
        .with_timezone(&chrono::Utc)
}

/// Continuous readings for one slot, as `/readings/batch` accepts them.
fn continuous(site_id: &str, parameter_id: &str, points: &[(&str, f64)]) -> Vec<serde_json::Value> {
    points
        .iter()
        .map(|(hhmm, value)| {
            json!({
                "site_id": site_id,
                "parameter_id": parameter_id,
                "time": format!("{DAY}T{hhmm}:00Z"),
                "raw_value": value,
                "measurement_type": "continuous",
            })
        })
        .collect()
}

async fn sensor_vs_grab(
    app: &Router,
    jwt: &str,
    site_id: &str,
    parameter_id: &str,
    extra: &str,
) -> (u16, serde_json::Value) {
    let uri = format!(
        "/api/sites/{site_id}/export/sensor-vs-grab?parameter_id={parameter_id}\
         &start={DAY}T00:00:00Z&end={DAY}T23:59:59Z{extra}"
    );
    crate::common::get_json_with_token(app, &uri, jwt).await
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn doc_tool_replicates_saved_at_a_station_reproduce_the_tool_statistics() {
    if !kc::require_keycloak_or_skip("doc_tool_replicates_saved_at_a_station").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_grab_track(&app, &admin).await;
    let parameter_id = track.parameter_id("TrkGrabDoc").to_string();
    let sensor_id = track
        .sensor_id
        .clone()
        .expect("the grab track provisions a lab instrument");
    let intern = member(&db, &track.project_id, "intern1", "riverdata-intern").await;
    let river = member(&db, &track.project_id, "river1", "riverdata-river").await;

    let doc = portal_doc_triplicate("S04", "2021-07-05");
    let (slope, intercept) = portal_curve("DOC corr");
    assert_eq!(
        (slope, intercept),
        (1.0, 0.0),
        "the DOC correction the portal recorded is the 1:1 pair, which is what lets the \
         exact-value assertions below compare the tool against the portal's own columns"
    );

    let (status, curve) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sensor_calibrations",
        &json!({
            "sensor_id": sensor_id,
            "slope": slope,
            "intercept": intercept,
            "valid_from": "2021-01-28T00:00:00Z",
            "mode": "instant",
            "name": "DOC corr",
        }),
        &admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "the lab instrument takes a standard curve ({status}): {curve}"
    );
    let curve_id = e2e::id_of(&curve);

    let (status, tool) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &json!({
            "DOC": [doc.replicates[0], doc.replicates[1], doc.replicates[2]],
            "std_curve": { "slope": slope, "intercept": intercept },
        }),
        &intern,
    )
    .await;
    assert_eq!(
        status, 200,
        "an intern may run an analytical tool ({status}): {tool}"
    );
    let tool_avg = f64_at(&tool["results"], "DOC_avg_ppb");
    let tool_sd = f64_at(&tool["results"], "DOC_sd_ppb");

    // The portal's own DOC_avg_ppb / DOC_sd_ppb columns for this triplicate are what the ported
    // calculator has to land on. The fixture carries six significant digits.
    assert!(
        (tool_avg - doc.recorded_avg).abs() < 1e-3,
        "tool average {tool_avg} must match the portal's recorded {} for {:?}",
        doc.recorded_avg,
        doc.replicates
    );
    assert!(
        (tool_sd - doc.recorded_sd).abs() < 1e-3,
        "tool sd {tool_sd} must match the portal's recorded {} for {:?}",
        doc.recorded_sd,
        doc.replicates
    );

    let corrected: Vec<f64> = doc
        .replicates
        .iter()
        .map(|r| slope * r + intercept)
        .collect();
    let expected_avg = corrected.iter().sum::<f64>() / corrected.len() as f64;
    assert!(
        (tool_avg - expected_avg).abs() < 1e-9,
        "the tool averages the curve-corrected replicates: {tool_avg} vs {expected_avg}"
    );

    let at = "2021-07-05T09:20:00Z";
    let readings: Vec<serde_json::Value> = doc
        .replicates
        .iter()
        .enumerate()
        .map(|(i, value)| {
            json!({
                "parameter_id": parameter_id,
                "sensor_id": sensor_id,
                "time": at,
                "value": value,
                "replicate_index": i as i16,
            })
        })
        .collect();
    let body = json!({ "site_id": track.site_id, "label": "DOC plate", "readings": readings });

    let (status, refused) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &body, &intern).await;
    assert_eq!(
        status, 403,
        "grab entry is data curation, an intern must not reach it: {refused}"
    );

    let (status, saved) =
        crate::common::post_json_parse_with_token(&app, "/api/grab_samples", &body, &river).await;
    assert_eq!(
        status, 200,
        "a river member saves the grab ({status}): {saved}"
    );
    assert_eq!(saved["inserted"], 3, "one reading per replicate: {saved}");
    assert_eq!(
        saved["samples_created"], 1,
        "the triplicate is one sample: {saved}"
    );

    let stored = {
        use sea_orm::{ConnectionTrait, Statement};
        db.query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT replicate_index, raw_value, calibrated_value, calibration_id, measurement_type \
                 FROM readings WHERE site_id = '{}' AND parameter_id = '{parameter_id}' \
                 ORDER BY replicate_index",
                track.site_id
            ),
        ))
        .await
        .expect("query readings")
    };
    assert_eq!(stored.len(), 3, "the fan-out writes one row per replicate");
    for (i, row) in stored.iter().enumerate() {
        let index: i16 = row.try_get("", "replicate_index").expect("replicate_index");
        let raw: f64 = row.try_get("", "raw_value").expect("raw_value");
        let calibrated: f64 = row
            .try_get("", "calibrated_value")
            .expect("calibrated_value");
        let stamped: uuid::Uuid = row.try_get("", "calibration_id").expect("calibration_id");
        let mtype: String = row
            .try_get("", "measurement_type")
            .expect("measurement_type");
        assert_eq!(
            index, i as i16,
            "the replicate_index the request supplied is the one stored"
        );
        assert!(
            (raw - doc.replicates[i]).abs() < 1e-9,
            "replicate {i} stores the measured value, got {raw}"
        );
        assert!(
            (calibrated - corrected[i]).abs() < 1e-9,
            "replicate {i} stores slope * raw + intercept, got {calibrated} not {}",
            corrected[i]
        );
        assert_eq!(
            stamped.to_string(),
            curve_id,
            "the applied curve is stamped on every replicate for provenance"
        );
        assert_eq!(mtype, "spot", "grab readings are spot data");
    }

    let sample = {
        use sea_orm::{ConnectionTrait, Statement};
        db.query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT n, mean, stdev, min_value, max_value FROM samples \
                 WHERE site_id = '{}' AND parameter_id = '{parameter_id}' AND collected_at = '{at}'",
                track.site_id
            ),
        ))
        .await
        .expect("query samples")
        .expect("the triplicate has a samples row")
    };
    let n: i32 = sample.try_get("", "n").expect("n");
    let mean: f64 = sample.try_get("", "mean").expect("mean");
    let stdev: f64 = sample.try_get("", "stdev").expect("stdev");
    let min: f64 = sample.try_get("", "min_value").expect("min_value");
    let max: f64 = sample.try_get("", "max_value").expect("max_value");
    assert_eq!(n, 3, "all three replicates count towards the sample");
    assert!(
        (mean - tool_avg).abs() < 1e-9,
        "the trigger's mean is the tool's own average: {mean} vs {tool_avg}"
    );
    assert!(
        (stdev - tool_sd).abs() < 1e-9,
        "STDDEV_SAMP reproduces the tool's Bessel-corrected sd: {stdev} vs {tool_sd}"
    );
    let expected_min = corrected.iter().copied().fold(f64::INFINITY, f64::min);
    let expected_max = corrected.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    assert!(
        (min - expected_min).abs() < 1e-9 && (max - expected_max).abs() < 1e-9,
        "sample bounds span the corrected replicates, got {min}..{max}"
    );

    let (status, served) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/sites/{}/readings?parameter_ids={parameter_id}\
             &start=2021-07-05T00:00:00Z&end=2021-07-05T23:59:59Z\
             &measurement_type=spot&include_sample_stats=true",
            track.site_id
        ),
        &intern,
    )
    .await;
    assert_eq!(
        status, 200,
        "an intern reads the spot series ({status}): {served}"
    );

    let values = e2e::values_for(&served, &parameter_id);
    assert_eq!(
        values.len(),
        1,
        "a replicate group is served as one point, not three: {served}"
    );
    assert!(
        (values[0] - tool_avg).abs() < 1e-9,
        "the served point is the group mean, ie. the tool's own average: {} vs {tool_avg}",
        values[0]
    );
    assert!(
        (BAND_GRAB.0..BAND_GRAB.1).contains(&values[0]),
        "the served value sits in the grab track's band: {}",
        values[0]
    );

    let block = parameter_block(&served, &parameter_id);
    let stats = block["samples"]
        .as_array()
        .unwrap_or_else(|| panic!("include_sample_stats attaches a samples array: {served}"));
    assert_eq!(stats.len(), 1, "one stats entry per served point: {served}");
    assert!(
        stats[0].is_object(),
        "the served spot point carries its sample statistics: {served}"
    );
    let stat = &stats[0];
    assert_eq!(
        stat["n"], 3,
        "the attached stats count three replicates: {stat}"
    );
    assert!(
        (f64_at(stat, "mean") - tool_avg).abs() < 1e-9,
        "the attached mean is the tool's average: {stat}"
    );
    let replicates = stat["replicates"]
        .as_array()
        .unwrap_or_else(|| panic!("the stats carry the individual replicates: {stat}"));
    assert_eq!(replicates.len(), 3, "every replicate is surfaced: {stat}");
    for (i, replicate) in replicates.iter().enumerate() {
        assert_eq!(
            replicate["replicate_index"].as_i64(),
            Some(i as i64),
            "replicates come back at the indices they were saved under: {replicate}"
        );
        assert!(
            (f64_at(replicate, "raw_value") - doc.replicates[i]).abs() < 1e-9,
            "replicate {i} keeps its measured value: {replicate}"
        );
        assert!(
            (f64_at(replicate, "calibrated_value") - corrected[i]).abs() < 1e-9,
            "replicate {i} keeps its curve-corrected value: {replicate}"
        );
        assert_eq!(
            replicate["flagged"], false,
            "a freshly saved replicate is unflagged: {replicate}"
        );
    }
}

#[tokio::test]
#[serial]
async fn sensor_vs_grab_window_edges_are_inclusive_and_configurable() {
    if !kc::require_keycloak_or_skip("sensor_vs_grab_window_edges").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_grab_track(&app, &admin).await;
    let cdom = add_parameter(&app, &admin, &track, "TrkGrabCdom", "Track Grab CDOM").await;
    let intern = member(&db, &track.project_id, "intern1", "riverdata-intern").await;
    let river = member(&db, &track.project_id, "river1", "riverdata-river").await;

    // Around a 06:00 grab: 07:00 and 13:00 sit outside the default 2-6h window, 08:00 and 12:00 sit
    // exactly on its edges, 10:00 inside.
    let (status, batch) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/batch",
        &json!({
            "readings": continuous(
                &track.site_id,
                &cdom,
                &[("07:00", 300.0), ("08:00", 302.0), ("10:00", 304.0), ("12:00", 306.0), ("13:00", 350.0)],
            )
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "continuous batch ({status}): {batch}");
    assert_eq!(
        batch["inserted"], 5,
        "all five continuous points land: {batch}"
    );

    let (status, grab) = crate::common::post_json_parse_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": track.site_id,
            "readings": tracks::grab_replicates(&cdom, &format!("{DAY}T06:00:00Z"), &[312.0, 314.0]),
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "grab entry ({status}): {grab}");
    assert_eq!(grab["inserted"], 2, "both replicates land: {grab}");
    assert_eq!(grab["samples_created"], 1, "the pair is one sample: {grab}");

    let (status, default) = sensor_vs_grab(&app, &intern, &track.site_id, &cdom, "").await;
    assert_eq!(status, 200, "default window ({status}): {default}");
    assert!(
        (f64_at(&default, "window_start_hours") - 2.0).abs() < 1e-9
            && (f64_at(&default, "window_end_hours") - 6.0).abs() < 1e-9,
        "the export defaults to the portal's +2-6h pairing: {default}"
    );
    let rows = default["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 1, "one grab in range: {default}");
    let row = &rows[0];
    assert_eq!(
        time_at(row),
        instant("06:00"),
        "the row is keyed by the grab's collection time: {row}"
    );
    assert_eq!(row["grab_n"], 2, "both replicates count: {row}");
    assert!(
        (f64_at(row, "grab_value") - 313.0).abs() < 1e-9,
        "grab_value is the replicate mean: {row}"
    );
    assert!(
        (f64_at(row, "grab_sd") - 2.0_f64.sqrt()).abs() < 1e-9,
        "grab_sd is the sample sd across replicates: {row}"
    );
    assert_eq!(
        row["sensor_n"], 3,
        "08:00 and 12:00 sit exactly on the window edges and are included, 07:00 and 13:00 are not: {row}"
    );
    assert!(
        (f64_at(row, "sensor_avg") - 304.0).abs() < 1e-9,
        "sensor_avg averages only 302/304/306; 300 or 350 leaking in would move it: {row}"
    );
    assert!(
        (f64_at(row, "sensor_sd") - 2.0).abs() < 1e-9,
        "sensor_sd is the sample sd over the same three points: {row}"
    );
    assert!(
        (f64_at(row, "difference") - 9.0).abs() < 1e-9,
        "difference is grab_value minus sensor_avg: {row}"
    );

    let (status, narrow) = sensor_vs_grab(
        &app,
        &intern,
        &track.site_id,
        &cdom,
        "&window_start_hours=3&window_end_hours=5",
    )
    .await;
    assert_eq!(status, 200, "narrowed window ({status}): {narrow}");
    assert!(
        (f64_at(&narrow, "window_start_hours") - 3.0).abs() < 1e-9
            && (f64_at(&narrow, "window_end_hours") - 5.0).abs() < 1e-9,
        "the response echoes the requested window: {narrow}"
    );
    let rows = narrow["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 1, "the grab is still in range: {narrow}");
    let row = &rows[0];
    assert_eq!(
        row["sensor_n"], 1,
        "only 10:00 falls in [09:00, 11:00]: {row}"
    );
    assert!(
        (f64_at(row, "sensor_avg") - 304.0).abs() < 1e-9,
        "sensor_avg is the single in-window reading: {row}"
    );
    assert!(
        row["sensor_sd"].is_null(),
        "a one-reading window has no sample standard deviation: {row}"
    );
    assert!(
        (f64_at(row, "difference") - 9.0).abs() < 1e-9,
        "difference still pairs the grab mean against the window average: {row}"
    );

    let (status, wide) = sensor_vs_grab(
        &app,
        &intern,
        &track.site_id,
        &cdom,
        "&window_start_hours=0&window_end_hours=8",
    )
    .await;
    assert_eq!(status, 200, "widened window ({status}): {wide}");
    let rows = wide["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 1, "still one grab: {wide}");
    let row = &rows[0];
    assert_eq!(
        row["sensor_n"], 5,
        "the two grab replicates sit at offset 0 in this window and are excluded as spot data: {row}"
    );
    assert!(
        (f64_at(row, "sensor_avg") - 312.4).abs() < 1e-9,
        "sensor_avg covers all five continuous points and nothing else: {row}"
    );
}

#[tokio::test]
#[serial]
async fn sensor_vs_grab_orders_grabs_and_reports_an_empty_post_grab_window() {
    if !kc::require_keycloak_or_skip("sensor_vs_grab_ordering").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_grab_track(&app, &admin).await;
    let cdom = add_parameter(&app, &admin, &track, "TrkGrabCdom", "Track Grab CDOM").await;
    let intern = member(&db, &track.project_id, "intern1", "riverdata-intern").await;
    let river = member(&db, &track.project_id, "river1", "riverdata-river").await;

    let (status, batch) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/batch",
        &json!({
            "readings": continuous(
                &track.site_id,
                &cdom,
                &[("08:00", 302.0), ("10:00", 304.0), ("12:00", 306.0)],
            )
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "continuous batch ({status}): {batch}");
    assert_eq!(
        batch["inserted"], 3,
        "three continuous points land: {batch}"
    );

    // The afternoon grab is entered second but collected later, so ordering cannot come from
    // insertion order alone.
    let (status, afternoon) = crate::common::post_json_parse_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": track.site_id,
            "notes": "single bottle",
            "readings": tracks::grab_replicates(&cdom, &format!("{DAY}T18:00:00Z"), &[320.0]),
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "single-bottle grab ({status}): {afternoon}");
    assert_eq!(
        afternoon["inserted"], 1,
        "the lone reading lands: {afternoon}"
    );
    assert_eq!(
        afternoon["samples_created"], 1,
        "a note gives even a single reading a sample to hang on: {afternoon}"
    );

    let (status, morning) = crate::common::post_json_parse_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": track.site_id,
            "readings": tracks::grab_replicates(&cdom, &format!("{DAY}T06:00:00Z"), &[312.0, 314.0]),
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "morning grab ({status}): {morning}");
    assert_eq!(
        morning["samples_created"], 1,
        "the pair is one sample: {morning}"
    );

    let (status, export) = sensor_vs_grab(&app, &intern, &track.site_id, &cdom, "").await;
    assert_eq!(status, 200, "comparison ({status}): {export}");
    let rows = export["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 2, "both grabs are paired: {export}");
    assert_eq!(
        time_at(&rows[0]),
        instant("06:00"),
        "rows are ordered by collection time, not by entry order: {export}"
    );
    assert_eq!(
        time_at(&rows[1]),
        instant("18:00"),
        "the later grab comes second: {export}"
    );

    assert_eq!(
        rows[0]["grab_n"], 2,
        "the morning grab has two replicates: {}",
        rows[0]
    );
    assert!(
        (f64_at(&rows[0], "grab_value") - 313.0).abs() < 1e-9,
        "the morning grab's value is its replicate mean: {}",
        rows[0]
    );
    assert_eq!(
        rows[0]["sensor_n"], 3,
        "its window holds three readings: {}",
        rows[0]
    );
    assert!(
        (f64_at(&rows[0], "sensor_avg") - 304.0).abs() < 1e-9,
        "its window average is exact: {}",
        rows[0]
    );

    assert_eq!(
        rows[1]["grab_n"], 1,
        "the afternoon grab has one replicate: {}",
        rows[1]
    );
    assert!(
        (f64_at(&rows[1], "grab_value") - 320.0).abs() < 1e-9,
        "its value is the single bottle: {}",
        rows[1]
    );
    assert!(
        rows[1]["grab_sd"].is_null(),
        "one replicate yields no sample standard deviation: {}",
        rows[1]
    );
    assert_eq!(
        rows[1]["sensor_n"], 0,
        "nothing continuous was logged in [20:00, 24:00]: {}",
        rows[1]
    );
    assert!(
        rows[1]["sensor_avg"].is_null() && rows[1]["sensor_sd"].is_null(),
        "an empty window reports no average rather than borrowing another grab's: {}",
        rows[1]
    );
    assert!(
        rows[1]["difference"].is_null(),
        "with no sensor side there is no difference: {}",
        rows[1]
    );

    let uri = format!(
        "/api/sites/{}/export/sensor-vs-grab?parameter_id={cdom}\
         &start={DAY}T00:00:00Z&end={DAY}T23:59:59Z&format=csv",
        track.site_id
    );
    let (status, csv) = crate::common::get_csv_with_token(&app, &uri, &intern).await;
    assert_eq!(status, 200, "comparison CSV ({status}): {csv}");
    let lines: Vec<&str> = csv.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 3, "header plus one line per grab:\n{csv}");
    assert_eq!(
        lines[0], "time,grab_value,grab_sd,grab_n,sensor_avg,sensor_sd,sensor_n,difference",
        "the CSV header is the documented column set"
    );
    let morning_cells: Vec<&str> = lines[1].split(',').collect();
    assert_eq!(morning_cells.len(), 8, "one cell per column: {}", lines[1]);
    assert_eq!(
        morning_cells[3], "2",
        "grab_n for the morning grab: {}",
        lines[1]
    );
    assert_eq!(
        morning_cells[6], "3",
        "sensor_n for the morning grab: {}",
        lines[1]
    );
    let afternoon_cells: Vec<&str> = lines[2].split(',').collect();
    assert_eq!(
        afternoon_cells.len(),
        8,
        "one cell per column: {}",
        lines[2]
    );
    assert_eq!(
        afternoon_cells[2], "",
        "a null grab_sd is an empty cell: {}",
        lines[2]
    );
    assert_eq!(
        afternoon_cells[3], "1",
        "grab_n for the afternoon grab: {}",
        lines[2]
    );
    assert_eq!(
        afternoon_cells[4], "",
        "a null sensor_avg is an empty cell: {}",
        lines[2]
    );
    assert_eq!(
        afternoon_cells[6], "0",
        "sensor_n is a real zero, not a blank: {}",
        lines[2]
    );
    assert_eq!(
        afternoon_cells[7], "",
        "a null difference is an empty cell: {}",
        lines[2]
    );
}

#[tokio::test]
#[serial]
async fn sensor_vs_grab_empty_comparisons_and_invalid_windows() {
    if !kc::require_keycloak_or_skip("sensor_vs_grab_edges").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_grab_track(&app, &admin).await;
    let cdom = add_parameter(&app, &admin, &track, "TrkGrabCdom", "Track Grab CDOM").await;
    let ungrabbed =
        add_parameter(&app, &admin, &track, "TrkGrabTurb", "Track Grab Turbidity").await;
    let intern = member(&db, &track.project_id, "intern1", "riverdata-intern").await;
    let river = member(&db, &track.project_id, "river1", "riverdata-river").await;

    let (status, batch) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/batch",
        &json!({
            "readings": continuous(
                &track.site_id,
                &cdom,
                &[("08:00", 302.0), ("10:00", 304.0), ("12:00", 306.0)],
            )
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "continuous batch ({status}): {batch}");

    let (status, grab) = crate::common::post_json_parse_with_token(
        &app,
        "/api/grab_samples",
        &json!({
            "site_id": track.site_id,
            "readings": tracks::grab_replicates(&cdom, &format!("{DAY}T06:00:00Z"), &[312.0, 314.0]),
        }),
        &river,
    )
    .await;
    assert_eq!(status, 200, "grab entry ({status}): {grab}");

    let (status, other) = sensor_vs_grab(&app, &intern, &track.site_id, &ungrabbed, "").await;
    assert_eq!(
        status, 200,
        "a parameter with no grabs is not an error ({status}): {other}"
    );
    assert_eq!(
        other["rows"].as_array().map(Vec::len),
        Some(0),
        "no grabs means no rows: {other}"
    );
    assert_eq!(
        other["parameter_id"].as_str(),
        Some(ungrabbed.as_str()),
        "the response still names the parameter it was asked about: {other}"
    );
    assert_eq!(
        other["site"]["id"].as_str(),
        Some(track.site_id.as_str()),
        "and the site: {other}"
    );

    let out_of_range = format!(
        "/api/sites/{}/export/sensor-vs-grab?parameter_id={cdom}\
         &start=2025-06-06T00:00:00Z&end=2025-06-06T23:59:59Z",
        track.site_id
    );
    let (status, empty) = crate::common::get_json_with_token(&app, &out_of_range, &intern).await;
    assert_eq!(status, 200, "a range holding no grabs ({status}): {empty}");
    assert_eq!(
        empty["rows"].as_array().map(Vec::len),
        Some(0),
        "the grab a day earlier is outside the requested range: {empty}"
    );

    let (status, csv) =
        crate::common::get_csv_with_token(&app, &format!("{out_of_range}&format=csv"), &intern)
            .await;
    assert_eq!(status, 200, "empty CSV ({status}): {csv}");
    let lines: Vec<&str> = csv.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "an empty comparison is still a valid CSV:\n{csv}"
    );
    assert_eq!(
        lines[0], "time,grab_value,grab_sd,grab_n,sensor_avg,sensor_sd,sensor_n,difference",
        "with the full header"
    );

    let (status, degenerate) = sensor_vs_grab(
        &app,
        &intern,
        &track.site_id,
        &cdom,
        "&window_start_hours=6&window_end_hours=6",
    )
    .await;
    assert_eq!(
        status, 400,
        "a zero-width window is rejected ({status}): {degenerate}"
    );
    assert_eq!(
        degenerate["error"].as_str(),
        Some("window_end_hours must be greater than window_start_hours"),
        "with a message naming both bounds: {degenerate}"
    );

    let (status, inverted) = sensor_vs_grab(
        &app,
        &intern,
        &track.site_id,
        &cdom,
        "&window_start_hours=6&window_end_hours=2",
    )
    .await;
    assert_eq!(
        status, 400,
        "an inverted window is rejected ({status}): {inverted}"
    );

    let (status, missing) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/sites/00000000-0000-4000-a000-999999999999/export/sensor-vs-grab\
             ?parameter_id={cdom}&start={DAY}T00:00:00Z&end={DAY}T23:59:59Z"
        ),
        &intern,
    )
    .await;
    assert_eq!(
        status, 404,
        "an unknown site is a 404 ({status}): {missing}"
    );

    kc::ensure_realm_user("norole", "norole", &[]).await;
    let norole = kc::get_keycloak_jwt("norole", "norole").await;
    let (status, denied) = sensor_vs_grab(&app, &norole, &track.site_id, &cdom, "").await;
    assert_eq!(
        status, 403,
        "a valid login without a riverdata role reads nothing ({status}): {denied}"
    );
}

#[tokio::test]
#[serial]
async fn doc_tool_rejects_malformed_input_and_accounts_for_every_request_key() {
    if !kc::require_keycloak_or_skip("doc_tool_input_contract").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db).await;
    kc::ensure_realm_user("intern1", "intern1", &["riverdata-intern"]).await;
    // The tools surface has no project dimension, so no grant is needed: the read_data capability
    // alone is what admits an intern here.
    let intern = kc::get_keycloak_jwt("intern1", "intern1").await;

    let (status, wrong_type) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &json!({ "DOC": ["not-a-number"] }),
        &intern,
    )
    .await;
    assert_eq!(
        status, 400,
        "a mistyped field is a bad request ({status}): {wrong_type}"
    );
    assert!(
        wrong_type["error"]
            .as_str()
            .is_some_and(|e| e.contains("Invalid request body")),
        "the error names the body as the problem: {wrong_type}"
    );

    // `required` is reserved for fields without which the tool cannot run. Omitting every doc
    // replicate reaches the wrapper as an absent series, which is the same uncomputable case as
    // all-null cells, so it answers like one rather than being refused.
    let (status, absent) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &json!({}),
        &intern,
    )
    .await;
    assert_eq!(
        status, 200,
        "an omitted optional series is the uncomputable case, not a bad request ({status}): \
         {absent}"
    );
    assert_eq!(
        absent["results"].as_object().map(|results| results.len()),
        Some(0),
        "an uncomputable result is omitted rather than serialized: {absent}"
    );

    // Discharge keeps its series required because an empty one makes the underlying lm() fail,
    // so the refusal that doc no longer owes is still pinned on a tool that owes it.
    let (status, incomplete) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/discharge/calculate",
        &json!({ "tracer": "salt", "values": [1.0, 2.0] }),
        &intern,
    )
    .await;
    assert_eq!(
        status, 400,
        "a body missing a genuinely required field is a bad request ({status}): {incomplete}"
    );
    assert!(
        incomplete["error"]
            .as_str()
            .is_some_and(|e| e.contains("times_s")),
        "the error names the missing field: {incomplete}"
    );

    let (status, empty) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &json!({ "DOC": [null, null, null] }),
        &intern,
    )
    .await;
    assert_eq!(
        status, 200,
        "an uncomputable calculation is not an error ({status}): {empty}"
    );
    assert_eq!(empty["tool"], "doc", "the response names the tool: {empty}");
    assert_eq!(
        empty["results"].as_object().map(|results| results.len()),
        Some(0),
        "an uncomputable result is omitted rather than serialized, so a save cannot clobber \
         stored data: {empty}"
    );

    // An unknown field is refused by name, not silently dropped: a misnamed field whose value
    // silently fell out of the calculation is how a wrong number gets saved without warning.
    let (status, unknown) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &json!({
            "DOC": [100.0, 104.0],
            "notes": "plate A",
        }),
        &intern,
    )
    .await;
    assert_eq!(
        status, 400,
        "an unknown field is a bad request ({status}): {unknown}"
    );
    assert!(
        unknown["error"]
            .as_str()
            .is_some_and(|e| e.contains("notes")),
        "the error names the unknown field: {unknown}"
    );

    let (status, full) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tools/doc/calculate",
        &json!({
            "DOC": [100.0, 104.0],
            "std_curve": { "slope": 2.0, "intercept": 1.0 },
        }),
        &intern,
    )
    .await;
    assert_eq!(
        status, 200,
        "a complete request calculates ({status}): {full}"
    );
    assert!(
        (f64_at(&full["results"], "DOC_avg_ppb") - 205.0).abs() < 1e-9,
        "the curve is applied per replicate before averaging: (201 + 209) / 2: {full}"
    );
    assert!(
        (f64_at(&full["results"], "DOC_sd_ppb") - 32.0_f64.sqrt()).abs() < 1e-9,
        "and the sd is taken over the corrected replicates: {full}"
    );

    let mut used: Vec<&str> = full["inputs_used"]
        .as_array()
        .unwrap_or_else(|| panic!("inputs_used is an array: {full}"))
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    used.sort_unstable();
    assert_eq!(
        used,
        ["DOC", "std_curve"],
        "every key the tool consumed is reported: {full}"
    );

    let ignored: Vec<&str> = full["inputs_ignored"]
        .as_array()
        .unwrap_or_else(|| panic!("inputs_ignored is an array: {full}"))
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert_eq!(
        ignored,
        Vec::<&str>::new(),
        "nothing sent went unconsumed: {full}"
    );
}
