//! Three onboarding tracks, one per real way data enters this system.
//!
//! Each builder provisions from nothing through the real HTTP surface, in the order the dashboard
//! drives it, and returns the ids it created. The builder is therefore itself the assertion that a
//! user can perform the flow; the SQL-level helpers in `sensor_lifecycle.rs` stay for tests that
//! need state rather than a journey.
//!
//! The three tracks are mutually disjoint in site, parameter, sensor, stream and value range, so a
//! downstream test that accidentally reads another track's rows fails instead of passing by
//! coincidence. Their stream provenance is not invented for the tests: `api` for CSV import,
//! `grab_sample` for grab entry and a registered source system for sync are what the three real
//! write paths already produce.
//!
//! | | A: CSV dump | B: Sensor flow | C: Grab and tools |
//! |---|---|---|---|
//! | ingestion | `POST /readings/import_csv` | repeated `POST /ingest` cycles | `POST /grab_samples` |
//! | provenance | `api` stream | registered stream, paired | `grab_sample` stream |
//! | sensor | none | sensor, calibration, deployment | lab instrument with a curve |
//! | values | 100-199 | 200-299 | 300-399 |

use axum::Router;
use serde_json::json;

use super::e2e;

/// Value band a track's readings occupy. Disjoint across tracks so cross-contamination is visible.
pub const BAND_CSV: (f64, f64) = (100.0, 200.0);
pub const BAND_FLOW: (f64, f64) = (200.0, 300.0);
pub const BAND_GRAB: (f64, f64) = (300.0, 400.0);

/// The sensor-flow track ingests in discrete cycles, mirroring a sync service's update loop.
pub const FLOW_CYCLES: usize = 5;
pub const FLOW_READINGS_PER_CYCLE: usize = 5;
/// Spacing between readings within a cycle, in data time. No wall-clock sleeping happens.
pub const FLOW_STEP_SECS: i64 = 2;

pub struct Track {
    pub project_id: String,
    pub site_id: String,
    /// (parameter code, parameter id) in creation order.
    pub parameters: Vec<(String, String)>,
    pub site_parameter_ids: Vec<String>,
    pub sensor_id: Option<String>,
    pub calibration_id: Option<String>,
    pub deployment_id: Option<String>,
    pub stream_ids: Vec<String>,
    pub band: (f64, f64),
}

impl Track {
    pub fn parameter_id(&self, code: &str) -> &str {
        &self
            .parameters
            .iter()
            .find(|(c, _)| c == code)
            .unwrap_or_else(|| panic!("track has no parameter {code}"))
            .1
    }
}

async fn provision(
    app: &Router,
    token: &str,
    slug: &str,
    params: &[(&str, &str, &str)],
) -> (String, String, Vec<(String, String)>, Vec<String>) {
    let project_id = e2e::create_project(
        app,
        token,
        &format!("Track {slug}"),
        &format!("trk-{slug}"),
        true,
    )
    .await;
    let site_id = e2e::create_site(
        app,
        token,
        &project_id,
        &format!("Site {slug}"),
        &format!("site-{slug}"),
    )
    .await;

    let mut parameters = Vec::new();
    let mut site_parameter_ids = Vec::new();
    for (code, name, units) in params {
        let pid = e2e::create_parameter(app, token, code, name, units).await;
        site_parameter_ids
            .push(e2e::assign_site_parameter_minimal(app, token, &site_id, &pid).await);
        parameters.push(((*code).to_string(), pid));
    }
    (project_id, site_id, parameters, site_parameter_ids)
}

/// Track A: a site provisioned from scratch, then a wide CSV imported against it.
///
/// No sensor is involved: CSV import attributes readings by site and parameter alone and creates
/// its own `api` stream per slot, which is the historical-upload path and is what makes this track
/// structurally distinct from the other two.
pub async fn onboard_csv_track(app: &Router, token: &str) -> Track {
    let (project_id, site_id, parameters, site_parameter_ids) = provision(
        app,
        token,
        "csv",
        &[
            ("TrkCsvDepth", "Track CSV Depth", "mm"),
            ("TrkCsvTurb", "Track CSV Turbidity", "NTU"),
        ],
    )
    .await;

    Track {
        project_id,
        site_id,
        parameters,
        site_parameter_ids,
        sensor_id: None,
        calibration_id: None,
        deployment_id: None,
        stream_ids: Vec::new(),
        band: BAND_CSV,
    }
}

/// A wide CSV in the shape the importer accepts: a `DateTime` column plus one column per parameter.
///
/// Values stay inside the track's band so a row leaking into another track's assertions is visible.
pub fn csv_body(codes: &[&str], rows: usize, base: &str) -> String {
    let mut out = String::from("DateTime");
    for c in codes {
        out.push(',');
        out.push_str(c);
    }
    out.push('\n');
    for i in 0..rows {
        let minutes = i * 10;
        out.push_str(&format!(
            "{base}T{:02}:{:02}:00Z",
            minutes / 60,
            minutes % 60
        ));
        for (j, _) in codes.iter().enumerate() {
            out.push_str(&format!(
                ",{:.2}",
                BAND_CSV.0 + (i * codes.len() + j) as f64 % 90.0
            ));
        }
        out.push('\n');
    }
    out
}

/// Track B: full sensor provisioning, a registered stream, then repeated ingest cycles.
///
/// The deployment is opened before the first cycle so readings arriving after pairing are stamped
/// with sensor, calibration and deployment at write time rather than by a later backfill.
pub async fn onboard_sensor_flow_track(app: &Router, token: &str) -> Track {
    let (project_id, site_id, parameters, site_parameter_ids) = provision(
        app,
        token,
        "flow",
        &[("TrkFlowDO", "Track Flow Dissolved Oxygen", "uM")],
    )
    .await;
    let parameter_id = parameters[0].1.clone();

    let sensor_id = e2e::create_sensor(app, token, &parameter_id, "TRK-FLOW-0001").await;
    let deployment_id = e2e::create_deployment(
        app,
        token,
        &sensor_id,
        &site_id,
        &parameter_id,
        &format!("{FLOW_BASE_DAY}T00:00:00Z"),
    )
    .await;

    let (status, stream) = super::post_json_parse_with_token(
        app,
        "/api/streams/register",
        &json!({
            "source_system": "trk_flow",
            "source_key": "trk-flow-do-1",
            "source_name": "Track flow DO",
            "sensor_id": sensor_id,
        }),
        token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "register stream ({status}): {stream}"
    );
    let stream_id = e2e::id_of(&stream);

    let calibration_id = super::e2e::identity_calibration_id(app, token, &sensor_id).await;

    Track {
        project_id,
        site_id,
        parameters,
        site_parameter_ids,
        sensor_id: Some(sensor_id),
        calibration_id,
        deployment_id: Some(deployment_id),
        stream_ids: vec![stream_id],
        band: BAND_FLOW,
    }
}

/// The day every sensor-flow fixture sits on. Past-dated: the aggregate refresh window is
/// `[since, NOW()]`, so a future-dated fixture is never materialised.
pub const FLOW_BASE_DAY: &str = "2025-06-02";

/// Readings for one ingest cycle, spaced `FLOW_STEP_SECS` apart in data time.
pub fn flow_cycle_readings(cycle: usize) -> Vec<serde_json::Value> {
    (0..FLOW_READINGS_PER_CYCLE)
        .map(|i| {
            let offset = (cycle * FLOW_READINGS_PER_CYCLE + i) as i64 * FLOW_STEP_SECS;
            json!({
                "time": format!("{FLOW_BASE_DAY}T00:{:02}:{:02}Z", offset / 60, offset % 60),
                "raw_value": BAND_FLOW.0 + (cycle * FLOW_READINGS_PER_CYCLE + i) as f64,
            })
        })
        .collect()
}

/// Track C: a site plus a lab instrument carrying a standard curve, fed by grab entry.
///
/// The instrument is a sensor like any other, which is how field and lab devices are modelled, but
/// it is never deployed to a slot: grab readings carry their curve through `calibration_id` per
/// reading instead.
pub async fn onboard_grab_track(app: &Router, token: &str) -> Track {
    let (project_id, site_id, parameters, site_parameter_ids) = provision(
        app,
        token,
        "grab",
        &[("TrkGrabDoc", "Track Grab DOC", "ppb")],
    )
    .await;

    let sensor_id = e2e::create_sensor(app, token, &parameters[0].1, "TRK-GRAB-0001").await;
    let calibration_id = super::e2e::identity_calibration_id(app, token, &sensor_id).await;

    Track {
        project_id,
        site_id,
        parameters,
        site_parameter_ids,
        sensor_id: Some(sensor_id),
        calibration_id,
        deployment_id: None,
        stream_ids: Vec::new(),
        band: BAND_GRAB,
    }
}

/// Replicate readings for one grab sample: `n` replicates of the same parameter at one instant.
pub fn grab_replicates(parameter_id: &str, at: &str, values: &[f64]) -> Vec<serde_json::Value> {
    values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            json!({
                "parameter_id": parameter_id,
                "time": at,
                "value": v,
                "replicate_index": i as i16,
            })
        })
        .collect()
}
