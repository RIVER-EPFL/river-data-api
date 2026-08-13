//! One calibration window spanning two deployments at two different sites.
//!
//! Scenario: a sonde measures one parameter at an upstream site for the morning, is moved
//! downstream at midday, and a single calibration covers both halves of the day. The operator later
//! corrects that calibration's coefficients.
//! Expected behaviour: the correction reaches the readings served at BOTH sites and the hourly
//! buckets at BOTH sites, because the rewrite is keyed on sensor and time and the aggregate refresh
//! is keyed on time alone. The mirror case is also pinned: moving the deployment boundary moves
//! which site owns a reading without touching a single calibrated value.
//!
//! The fixture is built from Track B through the real HTTP surface, extended with a second site and
//! a second deployment, and driven by real Keycloak users at the lowest role each step admits.

use serial_test::serial;
use std::time::Duration;

use axum::Router;
use sea_orm::DatabaseConnection;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::e2e;
use crate::common::keycloak as kc;
use crate::common::sensor_lifecycle as sl;
use crate::common::tracks;

const WAIT: Duration = Duration::from_secs(30);

/// Track B sits on 2025-06-02; the deployment it opens starts at midnight of that day.
const D1_FROM: &str = "2025-06-02T00:00:00Z";
/// The sensor moves downstream at midday.
const D2_FROM: &str = "2025-06-02T12:00:00Z";
/// The base identity curve predates every reading, so the span calibration is the only edit under test.
const BASE_CAL_FROM: &str = "2025-06-01T00:00:00Z";

const R1: &str = "2025-06-02T10:00:00Z";
const R2: &str = "2025-06-02T10:20:00Z";
const R3: &str = "2025-06-02T10:40:00Z";
const R4: &str = "2025-06-02T14:00:00Z";
const R5: &str = "2025-06-02T14:20:00Z";
const R6: &str = "2025-06-02T14:40:00Z";

const BUCKET_MORNING: &str = "2025-06-02T10:00:00Z";
const BUCKET_AFTERNOON: &str = "2025-06-02T14:00:00Z";

const WINDOW_START: &str = "2025-06-01T00:00:00Z";
const WINDOW_END: &str = "2025-06-03T00:00:00Z";

/// Three readings each side of the midday move. Raw values are round numbers so every calibrated
/// value and every bucket mean is exact in binary floating point.
const SPAN_READINGS: [(&str, f64); 6] = [
    (R1, 10.0),
    (R2, 20.0),
    (R3, 30.0),
    (R4, 40.0),
    (R5, 50.0),
    (R6, 60.0),
];

/// The partial-window variant drops the readings that would sit exactly on a window edge, leaving
/// one upstream reading before the span calibration starts and one after it.
const PARTIAL_READINGS: [(&str, f64); 4] = [(R1, 10.0), (R3, 30.0), (R4, 40.0), (R6, 60.0)];

/// A second sensor on a second parameter at the upstream site: never edited, never enqueued for.
const CONTROL_READINGS: [(&str, f64); 3] = [(R1, 2.0), (R2, 4.0), (R3, 6.0)];

fn as_uuid(s: &str) -> Uuid {
    s.parse()
        .unwrap_or_else(|e| panic!("expected a uuid, got '{s}': {e}"))
}

fn assert_close(actual: f64, expected: f64, what: &str) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "{what}: expected {expected}, got {actual}"
    );
}

fn ingest_body(stream: &str, readings: &[(&str, f64)]) -> Value {
    json!({
        "stream_id": stream,
        "readings": readings
            .iter()
            .map(|(t, v)| json!({ "time": t, "raw_value": v }))
            .collect::<Vec<_>>(),
    })
}

/// The index of a bucket in an aggregate response's `times`, or a panic carrying the body.
fn bucket_index(resp: &Value, bucket: &str) -> usize {
    resp["times"]
        .as_array()
        .unwrap_or_else(|| panic!("no 'times' array in aggregate response: {resp}"))
        .iter()
        .position(|t| t.as_str() == Some(bucket))
        .unwrap_or_else(|| panic!("bucket {bucket} absent from times: {resp}"))
}

/// Whether a bucket appears at all. A site with no reading in an hour has no entry in `times`
/// rather than a null value, so absence is what "this site holds nothing here" looks like.
fn has_bucket(resp: &Value, bucket: &str) -> bool {
    resp["times"]
        .as_array()
        .is_some_and(|a| a.iter().any(|t| t.as_str() == Some(bucket)))
}

/// One `split_by_sensor=true` series, matched on (parameter, sensor). A series with no owning
/// sensor omits the key entirely, so it can never match a sensor id.
fn split_series<'a>(resp: &'a Value, parameter_id: &str, sensor_id: &str) -> &'a Value {
    resp["parameters"]
        .as_array()
        .unwrap_or_else(|| panic!("no 'parameters' array in split response: {resp}"))
        .iter()
        .find(|p| p["parameter_id"] == parameter_id && p["sensor_id"] == sensor_id)
        .unwrap_or_else(|| {
            panic!("no series for parameter {parameter_id} sensor {sensor_id}: {resp}")
        })
}

fn series_number(series: &Value, field: &str, index: usize) -> f64 {
    series[field]
        .as_array()
        .unwrap_or_else(|| panic!("'{field}' is not an array: {series}"))
        .get(index)
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("'{field}'[{index}] is not a number: {series}"))
}

struct Span {
    app: Router,
    db: DatabaseConnection,
    manager: String,
    river: String,
    intern: String,
    site1: String,
    site2: String,
    parameter: String,
    sensor: String,
    stream: String,
    base_cal: String,
    span_cal: String,
    d1: String,
    d2: String,
    control_parameter: String,
    control_sensor: String,
    control_stream: String,
    control_cal: String,
}

impl Span {
    async fn readings(&self) -> Vec<sl::ReadingRow> {
        sl::get_readings(&self.db, as_uuid(&self.stream)).await
    }

    async fn control_readings(&self) -> Vec<sl::ReadingRow> {
        sl::get_readings(&self.db, as_uuid(&self.control_stream)).await
    }

    /// Served values for one parameter at one site, as an intern (the lowest role that may read data).
    async fn served(&self, site: &str, parameter: &str) -> Vec<f64> {
        let (status, body) = crate::common::get_json_with_token(
            &self.app,
            &format!(
                "/api/sites/{site}/readings?start={WINDOW_START}&end={WINDOW_END}&parameter_ids={parameter}"
            ),
            &self.intern,
        )
        .await;
        assert_eq!(status, 200, "readings at site {site} ({status}): {body}");
        e2e::values_for(&body, parameter)
    }

    async fn aggregates(&self, site: &str, extra: &str) -> Value {
        let (status, body) = crate::common::get_json_with_token(
            &self.app,
            &format!(
                "/api/sites/{site}/aggregates/hourly?start={WINDOW_START}&end={WINDOW_END}{extra}"
            ),
            &self.intern,
        )
        .await;
        assert_eq!(status, 200, "aggregates at site {site} ({status}): {body}");
        body
    }

    async fn calibration_window(&self, calibration: &str) -> Value {
        let (status, body) = crate::common::get_json_with_token(
            &self.app,
            &format!("/api/sensor_calibrations/{calibration}/window"),
            &self.intern,
        )
        .await;
        assert_eq!(status, 200, "calibration window ({status}): {body}");
        body
    }

    async fn sensor_identity(&self, site: &str) -> Value {
        let (status, body) = crate::common::get_json_with_token(
            &self.app,
            &format!("/api/sites/{site}/sensor_identity?start={WINDOW_START}&end={WINDOW_END}"),
            &self.intern,
        )
        .await;
        assert_eq!(
            status, 200,
            "sensor identity at site {site} ({status}): {body}"
        );
        body
    }

    /// Every calibrated value the control sensor holds, plus its provenance, unchanged throughout.
    async fn assert_control_untouched(&self, when: &str) {
        let rows = self.control_readings().await;
        assert_eq!(
            rows.len(),
            3,
            "{when}: the control sensor keeps its three readings"
        );
        for (i, expected) in [8.0, 16.0, 24.0].iter().enumerate() {
            assert_eq!(
                rows[i].calibrated_value,
                Some(*expected),
                "{when}: control row {i} keeps 4*raw"
            );
            assert_eq!(
                rows[i].calibration_id,
                Some(as_uuid(&self.control_cal)),
                "{when}: control row {i} keeps its own curve"
            );
            assert_eq!(
                rows[i].sensor_id,
                Some(as_uuid(&self.control_sensor)),
                "{when}: control row {i} keeps its own sensor"
            );
            assert_eq!(
                rows[i].site_id,
                Some(as_uuid(&self.site1)),
                "{when}: control row {i} stays upstream"
            );
        }
        let served = self.served(&self.site1, &self.control_parameter).await;
        assert_eq!(served.len(), 3, "{when}: three control values are served");
        for (i, expected) in [8.0, 16.0, 24.0].iter().enumerate() {
            assert_close(
                served[i],
                *expected,
                &format!("{when}: served control value {i}"),
            );
        }
    }
}

/// Track B, extended with a second site and a second deployment, and one calibration whose window
/// starts at `span_cal_from`.
///
/// Every entity is provisioned over HTTP in dashboard order. Readings are ingested before the span
/// calibration exists (ingest writes `calibrated_value` as raw), so the calibration's own create
/// job is what establishes the pre-state, exactly as it would for an operator entering a curve
/// against data that already arrived.
async fn seed_two_site_span(span_cal_from: &str, readings: &[(&str, f64)]) -> Span {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak_admin(db.clone()).await;

    for (user, password, role) in [
        ("manager1", "manager1", "riverdata-manager"),
        ("river1", "river1", "riverdata-river"),
        ("intern1", "intern1", "riverdata-intern"),
    ] {
        kc::ensure_realm_user(user, password, &[role]).await;
    }
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    let manager = kc::get_keycloak_jwt("manager1", "manager1").await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;
    let intern = kc::get_keycloak_jwt("intern1", "intern1").await;

    let track = tracks::onboard_sensor_flow_track(&app, &admin).await;
    let parameter = track.parameter_id("TrkFlowDO").to_string();
    let site1 = track.site_id.clone();
    let sensor = track
        .sensor_id
        .clone()
        .expect("track B provisions a sensor");
    let stream = track.stream_ids[0].clone();
    let d1 = track
        .deployment_id
        .clone()
        .expect("track B opens the upstream deployment");

    for user in ["manager1", "river1", "intern1"] {
        let sub = kc::keycloak_user_id(user).await;
        let (status, body) = crate::common::put_json_with_token(
            &app,
            &format!("/api/users/{sub}/grants"),
            &json!({ "project_ids": [track.project_id.as_str()] }),
            &admin,
        )
        .await;
        assert_eq!(
            status, 200,
            "admin grants {user} the track project ({status}): {body}"
        );
    }

    let site2 = e2e::create_site(
        &app,
        &river,
        &track.project_id,
        "Span Downstream",
        "span-down",
    )
    .await;
    // The downstream site carries the same global parameter, so the same stream's rows are servable
    // there once a deployment moves them.
    e2e::assign_site_parameter_minimal(&app, &manager, &site2, &parameter).await;

    let base_cal =
        create_calibration(&app, &manager, &sensor, &parameter, 1.0, 0.0, BASE_CAL_FROM).await;

    // A minted sensor's readings would be invisible to the sensor-scoped reprocess that applies the
    // span curve, so the link keeps one instrument, one deployment history and one calibration
    // timeline behind one feed.
    e2e::link_stream_sensor(&app, &admin, &stream, &sensor).await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream}/pair"),
        &json!({ "site_parameter_id": track.site_parameter_ids[0] }),
        &admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "pair the stream ({status}): {body}"
    );

    let move_body = json!({
        "sensor_id": sensor,
        "site_id": site2,
        "parameter_id": parameter,
        "deployed_from": D2_FROM,
    });
    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/sensor_deployments", &move_body, &river)
            .await;
    assert_eq!(
        status, 403,
        "moving a sensor between sites is sensor management, above the river level ({status}): {body}"
    );
    let (status, moved) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sensor_deployments",
        &move_body,
        &manager,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "manager moves the sensor ({status}): {moved}"
    );
    let d2 = e2e::id_of(&moved);

    let body_in = ingest_body(&stream, readings);
    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/ingest", &body_in, &intern).await;
    assert_eq!(
        status, 403,
        "an intern may read data but not push it ({status}): {body}"
    );
    let (status, ingested) =
        crate::common::post_json_parse_with_token(&app, "/api/ingest", &body_in, &river).await;
    assert_eq!(
        status, 200,
        "river ingests the span readings ({status}): {ingested}"
    );
    assert_eq!(
        ingested["inserted"],
        readings.len(),
        "every span reading lands: {ingested}"
    );

    let span_cal =
        create_calibration(&app, &manager, &sensor, &parameter, 2.0, 5.0, span_cal_from).await;
    assert!(
        sl::wait_for_reprocessing(&db, as_uuid(&sensor), WAIT).await,
        "the calibration_create job applies the span curve to the ingested readings"
    );

    let (control_parameter, control_sensor, control_stream, control_cal) =
        seed_control_sensor(&app, &db, &admin, &manager, &river, &site1).await;

    Span {
        app,
        db,
        manager,
        river,
        intern,
        site1,
        site2,
        parameter,
        sensor,
        stream,
        base_cal,
        span_cal,
        d1,
        d2,
        control_parameter,
        control_sensor,
        control_stream,
        control_cal,
    }
}

async fn create_calibration(
    app: &Router,
    token: &str,
    sensor: &str,
    parameter: &str,
    slope: f64,
    intercept: f64,
    valid_from: &str,
) -> String {
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/sensor_calibrations",
        &json!({
            "sensor_id": sensor,
            "parameter_id": parameter,
            "slope": slope,
            "intercept": intercept,
            "valid_from": valid_from,
        }),
        token,
    )
    .await;
    assert_eq!(
        status, 201,
        "create calibration {slope}x+{intercept} ({status}): {body}"
    );
    e2e::id_of(&body)
}

/// A second instrument on a second parameter at the upstream site, fully independent of the sensor
/// under test. It shares the upstream site and the same hour, so it also serves as the co-located
/// bucket that a time-scoped aggregate refresh must leave alone.
async fn seed_control_sensor(
    app: &Router,
    db: &DatabaseConnection,
    admin: &str,
    manager: &str,
    river: &str,
    site1: &str,
) -> (String, String, String, String) {
    let parameter =
        e2e::create_parameter(app, admin, "TrkSpanCtrl", "Track Span Control", "degC").await;
    let sp = e2e::assign_site_parameter_minimal(app, manager, site1, &parameter).await;
    let sensor = e2e::create_sensor(app, admin, &parameter, "TRK-SPAN-CTRL").await;
    // The deployment comes first: a calibration is confined to the projects its sensor is deployed
    // into, so a manager cannot author a curve for an undeployed instrument.
    e2e::create_deployment(app, manager, &sensor, site1, &parameter, D1_FROM).await;
    create_calibration(app, manager, &sensor, &parameter, 1.0, 0.0, BASE_CAL_FROM).await;

    let (status, stream) = crate::common::post_json_parse_with_token(
        app,
        "/api/streams/register",
        &json!({
            "source_system": "trk_flow",
            "source_key": "trk-span-control",
            "source_name": "Track span control",
            "sensor_id": sensor,
        }),
        admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "register control stream ({status}): {stream}"
    );
    let stream_id = e2e::id_of(&stream);

    let (status, body) = crate::common::post_json_with_token(
        app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({ "site_parameter_id": sp }),
        admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "pair control stream ({status}): {body}"
    );

    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/ingest",
        &ingest_body(&stream_id, &CONTROL_READINGS),
        river,
    )
    .await;
    assert_eq!(status, 200, "ingest control readings ({status}): {body}");

    let cal = create_calibration(app, manager, &sensor, &parameter, 4.0, 0.0, D1_FROM).await;
    assert!(
        sl::wait_for_reprocessing(db, as_uuid(&sensor), WAIT).await,
        "the control sensor's own curve is applied to its readings"
    );

    (parameter, sensor, stream_id, cal)
}

/// The six span readings under the pre-edit curve (2x + 5), with their site and deployment split.
async fn assert_span_pre_state(span: &Span) {
    let rows = span.readings().await;
    assert_eq!(
        rows.len(),
        6,
        "the span fixture holds six readings on one stream"
    );

    let expected = [
        (10.0, 25.0, &span.site1, &span.d1),
        (20.0, 45.0, &span.site1, &span.d1),
        (30.0, 65.0, &span.site1, &span.d1),
        (40.0, 85.0, &span.site2, &span.d2),
        (50.0, 105.0, &span.site2, &span.d2),
        (60.0, 125.0, &span.site2, &span.d2),
    ];
    for (i, (raw, calibrated, site, deployment)) in expected.iter().enumerate() {
        assert_eq!(rows[i].raw_value, *raw, "row {i} raw value");
        assert_eq!(
            rows[i].calibrated_value,
            Some(*calibrated),
            "row {i} carries 2*raw+5 before the edit"
        );
        assert_eq!(
            rows[i].calibration_id,
            Some(as_uuid(&span.span_cal)),
            "row {i} resolves to the spanning calibration"
        );
        assert_eq!(
            rows[i].site_id,
            Some(as_uuid(site.as_str())),
            "row {i} site"
        );
        assert_eq!(
            rows[i].deployment_id,
            Some(as_uuid(deployment.as_str())),
            "row {i} deployment"
        );
    }
}

async fn edit_span_calibration(span: &Span) {
    let patch = json!({ "slope": 3.0, "intercept": 1.0 });
    let path = format!("/api/sensor_calibrations/{}", span.span_cal);

    let (status, body) =
        crate::common::put_json_with_token(&span.app, &path, &patch, &span.river).await;
    assert_eq!(
        status, 403,
        "editing a calibration is sensor management, above the river level ({status}): {body}"
    );

    let (status, body) =
        crate::common::put_json_with_token(&span.app, &path, &patch, &span.manager).await;
    assert_eq!(
        status, 200,
        "manager edits the spanning calibration ({status}): {body}"
    );

    assert!(
        sl::wait_for_reprocessing(&span.db, as_uuid(&span.sensor), WAIT).await,
        "the calibration edit enqueues reprocessing for the sensor and it completes"
    );
    assert!(
        e2e::wait_for_jobs_by_trigger(&span.db, "calibration_update", 30).await,
        "the edit is tracked as a calibration_update job"
    );
}

#[tokio::test]
#[serial]
async fn calibration_edit_recalculates_readings_at_both_sites() {
    if !kc::require_keycloak_or_skip("calibration_edit_both_sites").await {
        return;
    }
    let span = seed_two_site_span(D1_FROM, &SPAN_READINGS).await;
    assert_span_pre_state(&span).await;

    let upstream_before = span.served(&span.site1, &span.parameter).await;
    assert_eq!(
        upstream_before.len(),
        3,
        "three upstream values are served before the edit"
    );
    for (i, expected) in [25.0, 45.0, 65.0].iter().enumerate() {
        assert_close(
            upstream_before[i],
            *expected,
            &format!("upstream value {i} before"),
        );
    }
    let downstream_before = span.served(&span.site2, &span.parameter).await;
    assert_eq!(
        downstream_before.len(),
        3,
        "three downstream values are served before the edit"
    );
    for (i, expected) in [85.0, 105.0, 125.0].iter().enumerate() {
        assert_close(
            downstream_before[i],
            *expected,
            &format!("downstream value {i} before"),
        );
    }

    edit_span_calibration(&span).await;

    let rows = span.readings().await;
    assert_eq!(rows.len(), 6, "the edit neither adds nor drops readings");
    let expected = [
        (10.0, 31.0, &span.site1, &span.d1),
        (20.0, 61.0, &span.site1, &span.d1),
        (30.0, 91.0, &span.site1, &span.d1),
        (40.0, 121.0, &span.site2, &span.d2),
        (50.0, 151.0, &span.site2, &span.d2),
        (60.0, 181.0, &span.site2, &span.d2),
    ];
    for (i, (raw, calibrated, site, deployment)) in expected.iter().enumerate() {
        assert_eq!(
            rows[i].raw_value, *raw,
            "row {i} raw value survives reprocessing"
        );
        assert_eq!(
            rows[i].calibrated_value,
            Some(*calibrated),
            "row {i} carries 3*raw+1 after the edit"
        );
        assert_eq!(
            rows[i].calibration_id,
            Some(as_uuid(&span.span_cal)),
            "row {i} still resolves to the same calibration, the edit changed coefficients not membership"
        );
        assert_eq!(
            rows[i].site_id,
            Some(as_uuid(site.as_str())),
            "row {i} keeps its site, a calibration edit does not touch the deployment axis"
        );
        assert_eq!(
            rows[i].deployment_id,
            Some(as_uuid(deployment.as_str())),
            "row {i} keeps its deployment"
        );
    }

    let upstream = span.served(&span.site1, &span.parameter).await;
    assert_eq!(
        upstream.len(),
        3,
        "three upstream values are served after the edit"
    );
    for (i, expected) in [31.0, 61.0, 91.0].iter().enumerate() {
        assert_close(upstream[i], *expected, &format!("upstream value {i} after"));
    }
    let downstream = span.served(&span.site2, &span.parameter).await;
    assert_eq!(
        downstream.len(),
        3,
        "three downstream values are served after the edit"
    );
    for (i, expected) in [121.0, 151.0, 181.0].iter().enumerate() {
        assert_close(
            downstream[i],
            *expected,
            &format!(
                "downstream value {i} after, a site-scoped rewrite would leave the old curve here"
            ),
        );
    }

    span.assert_control_untouched("after the calibration edit")
        .await;

    let window = span.calibration_window(&span.span_cal).await;
    assert_eq!(
        window["point_count"], 6,
        "one calibration owns readings from both sites: {window}"
    );
    assert_close(
        window["slope"].as_f64().unwrap_or(f64::NAN),
        3.0,
        "the calibration window reports the edited slope",
    );

    for (site, deployment) in [(&span.site1, &span.d1), (&span.site2, &span.d2)] {
        let identity = span.sensor_identity(site).await;
        let bands = identity["bands"][&span.parameter]
            .as_array()
            .unwrap_or_else(|| {
                panic!("no identity bands for the parameter at site {site}: {identity}")
            });
        assert!(
            bands
                .iter()
                .any(|b| b["deployment_id"] == deployment.as_str()),
            "site {site} shows its own deployment band: {identity}"
        );
        let markers = identity["calibrations"][&span.parameter]
            .as_array()
            .unwrap_or_else(|| {
                panic!("no calibration markers for the parameter at site {site}: {identity}")
            });
        let marker = markers
            .iter()
            .find(|m| m["calibration_id"] == span.span_cal.as_str())
            .unwrap_or_else(|| {
                panic!("the spanning calibration is not marked at site {site}: {identity}")
            });
        assert_close(
            marker["slope"].as_f64().unwrap_or(f64::NAN),
            3.0,
            &format!("marker slope at site {site}"),
        );
        assert_close(
            marker["intercept"].as_f64().unwrap_or(f64::NAN),
            1.0,
            &format!("marker intercept at site {site}"),
        );
    }
}

#[tokio::test]
#[serial]
async fn both_sites_aggregates_reflect_the_new_calibration() {
    if !kc::require_keycloak_or_skip("both_sites_aggregates").await {
        return;
    }
    let span = seed_two_site_span(D1_FROM, &SPAN_READINGS).await;
    assert_span_pre_state(&span).await;

    // Materialising the whole fixture range first is the precondition, not a convenience: it pushes
    // the aggregate's watermark past the readings, so a post-edit query is answered from the
    // materialised buckets. Without it a missing refresh could still be answered live from the
    // hypertable and the assertions below would pass with no refresh at all.
    e2e::refresh_hourly(&span.db, sl::dt(WINDOW_START)).await;

    let morning = sl::dt(BUCKET_MORNING);
    let afternoon = sl::dt(BUCKET_AFTERNOON);

    let before_up = e2e::hourly_bucket(&span.db, &span.site1, &span.parameter, morning).await;
    assert!(
        before_up.is_some(),
        "the upstream morning bucket is materialised before the edit"
    );
    let (mean, count) = before_up.unwrap();
    assert_close(mean, 45.0, "upstream morning mean before the edit");
    assert_eq!(
        count, 3,
        "upstream morning bucket holds three readings before the edit"
    );

    let before_down = e2e::hourly_bucket(&span.db, &span.site2, &span.parameter, afternoon).await;
    assert!(
        before_down.is_some(),
        "the downstream afternoon bucket is materialised before the edit"
    );
    let (mean, count) = before_down.unwrap();
    assert_close(mean, 105.0, "downstream afternoon mean before the edit");
    assert_eq!(
        count, 3,
        "downstream afternoon bucket holds three readings before the edit"
    );

    let before_ctrl =
        e2e::hourly_bucket(&span.db, &span.site1, &span.control_parameter, morning).await;
    assert!(
        before_ctrl.is_some(),
        "the co-located control bucket is materialised before the edit"
    );
    let (mean, count) = before_ctrl.unwrap();
    assert_close(mean, 16.0, "control mean before the edit");
    assert_eq!(
        count, 3,
        "control bucket holds three readings before the edit"
    );

    edit_span_calibration(&span).await;

    let after_up = e2e::hourly_bucket(&span.db, &span.site1, &span.parameter, morning).await;
    assert!(
        after_up.is_some(),
        "the upstream morning bucket survives the refresh"
    );
    let (mean, count) = after_up.unwrap();
    assert_close(mean, 61.0, "upstream morning mean tracks the new curve");
    assert_eq!(
        count, 3,
        "upstream morning bucket still holds three readings"
    );

    let after_down = e2e::hourly_bucket(&span.db, &span.site2, &span.parameter, afternoon).await;
    assert!(
        after_down.is_some(),
        "the downstream afternoon bucket survives the refresh"
    );
    let (mean, count) = after_down.unwrap();
    assert_close(
        mean,
        151.0,
        "the refresh is time-scoped, not site-scoped, so the downstream bucket moves too",
    );
    assert_eq!(
        count, 3,
        "downstream afternoon bucket still holds three readings"
    );

    let after_ctrl =
        e2e::hourly_bucket(&span.db, &span.site1, &span.control_parameter, morning).await;
    assert!(
        after_ctrl.is_some(),
        "the co-located control bucket survives the refresh"
    );
    let (mean, count) = after_ctrl.unwrap();
    assert_close(
        mean,
        16.0,
        "a co-located parameter fed by another sensor is undisturbed",
    );
    assert_eq!(count, 3, "control bucket still holds three readings");

    let up = span.aggregates(&span.site1, "").await;
    let idx = bucket_index(&up, BUCKET_MORNING);
    assert_close(
        e2e::field_for(&up, &span.parameter, "avg")[idx],
        61.0,
        "the upstream hourly average served over HTTP",
    );
    assert_close(
        e2e::field_for(&up, &span.parameter, "count")[idx],
        3.0,
        "the upstream hourly count served over HTTP",
    );
    assert_close(
        e2e::field_for(&up, &span.control_parameter, "avg")[idx],
        16.0,
        "the control average served over HTTP",
    );
    assert!(
        !has_bucket(&up, BUCKET_AFTERNOON),
        "the upstream site holds nothing in the afternoon hour: {up}"
    );

    let down = span.aggregates(&span.site2, "").await;
    let idx = bucket_index(&down, BUCKET_AFTERNOON);
    assert_close(
        e2e::field_for(&down, &span.parameter, "avg")[idx],
        151.0,
        "the downstream hourly average served over HTTP",
    );
    assert_close(
        e2e::field_for(&down, &span.parameter, "count")[idx],
        3.0,
        "the downstream hourly count served over HTTP",
    );
    assert!(
        !has_bucket(&down, BUCKET_MORNING),
        "the downstream site holds nothing in the morning hour: {down}"
    );

    let up_split = span.aggregates(&span.site1, "&split_by_sensor=true").await;
    let up_series = split_series(&up_split, &span.parameter, &span.sensor);
    let idx = bucket_index(&up_split, BUCKET_MORNING);
    assert_close(
        series_number(up_series, "avg", idx),
        61.0,
        "the per-sensor upstream series",
    );

    let down_split = span.aggregates(&span.site2, "&split_by_sensor=true").await;
    let down_series = split_series(&down_split, &span.parameter, &span.sensor);
    let idx = bucket_index(&down_split, BUCKET_AFTERNOON);
    assert_close(
        series_number(down_series, "avg", idx),
        151.0,
        "the same sensor's downstream series, keyed by the same sensor id under a second site",
    );

    let rows = span.readings().await;
    assert_eq!(
        rows.len(),
        6,
        "the readings behind the buckets are still six"
    );
    for (i, expected) in [31.0, 61.0, 91.0, 121.0, 151.0, 181.0].iter().enumerate() {
        assert_eq!(
            rows[i].calibrated_value,
            Some(*expected),
            "row {i} agrees with the bucket means, so a disagreement is an aggregate failure"
        );
    }
}

#[tokio::test]
#[serial]
async fn deployment_boundary_move_rebalances_sites_without_changing_values() {
    if !kc::require_keycloak_or_skip("deployment_boundary_move").await {
        return;
    }
    let span = seed_two_site_span(D1_FROM, &SPAN_READINGS).await;
    assert_span_pre_state(&span).await;

    e2e::refresh_hourly(&span.db, sl::dt(WINDOW_START)).await;
    let morning = sl::dt(BUCKET_MORNING);
    let afternoon = sl::dt(BUCKET_AFTERNOON);

    let before_up = e2e::hourly_bucket(&span.db, &span.site1, &span.parameter, morning).await;
    assert!(
        before_up.is_some(),
        "the upstream morning bucket is materialised before the move"
    );
    let (mean, count) = before_up.unwrap();
    assert_close(mean, 45.0, "upstream morning mean before the move");
    assert_eq!(count, 3, "three upstream readings before the move");
    assert!(
        e2e::hourly_bucket(&span.db, &span.site2, &span.parameter, morning)
            .await
            .is_none(),
        "the downstream site holds no morning bucket before the move"
    );

    let correction = json!({ "deployed_from": "2025-06-02T10:30:00Z" });
    let path = format!("/api/sensor_deployments/{}", span.d2);
    let (status, body) =
        crate::common::put_json_with_token(&span.app, &path, &correction, &span.river).await;
    assert_eq!(
        status, 403,
        "correcting a move date is sensor management, above the river level ({status}): {body}"
    );
    let (status, body) =
        crate::common::put_json_with_token(&span.app, &path, &correction, &span.manager).await;
    assert!(
        (200..300).contains(&status),
        "manager corrects the move to 10:30 ({status}): {body}"
    );

    assert!(
        sl::wait_for_reprocessing(&span.db, as_uuid(&span.sensor), WAIT).await,
        "the deployment edit enqueues reprocessing for the sensor and it completes"
    );
    assert!(
        e2e::wait_for_jobs_by_trigger(&span.db, "deployment_update", 30).await,
        "the edit is tracked as a deployment_update job"
    );

    // Precondition rather than a headline claim: the re-chaining of the earlier deployment's end is
    // pinned by tests/e2e/deployment_slot_and_recall.rs. It is read here because the whole
    // re-attribution below depends on it having happened.
    let (status, upstream_deployment) = crate::common::get_json_with_token(
        &span.app,
        &format!("/api/sensor_deployments/{}", span.d1),
        &span.intern,
    )
    .await;
    assert_eq!(
        status, 200,
        "read the upstream deployment ({status}): {upstream_deployment}"
    );
    let until = upstream_deployment["deployed_until"]
        .as_str()
        .unwrap_or_else(|| panic!("the upstream deployment must be closed: {upstream_deployment}"));
    assert_eq!(
        sl::dt(until),
        sl::dt("2025-06-02T10:30:00Z"),
        "the upstream deployment ends where the corrected move begins: {upstream_deployment}"
    );

    let rows = span.readings().await;
    assert_eq!(rows.len(), 6, "the move neither adds nor drops readings");
    let expected = [
        (25.0, &span.site1, &span.d1),
        (45.0, &span.site1, &span.d1),
        (65.0, &span.site2, &span.d2),
        (85.0, &span.site2, &span.d2),
        (105.0, &span.site2, &span.d2),
        (125.0, &span.site2, &span.d2),
    ];
    for (i, (calibrated, site, deployment)) in expected.iter().enumerate() {
        assert_eq!(
            rows[i].calibrated_value,
            Some(*calibrated),
            "row {i} keeps 2*raw+5, the deployment axis does not touch the curve"
        );
        assert_eq!(
            rows[i].calibration_id,
            Some(as_uuid(&span.span_cal)),
            "row {i} keeps the spanning calibration through the move"
        );
        assert_eq!(
            rows[i].site_id,
            Some(as_uuid(site.as_str())),
            "row {i} site after the move"
        );
        assert_eq!(
            rows[i].deployment_id,
            Some(as_uuid(deployment.as_str())),
            "row {i} deployment after the move"
        );
    }

    let sensor_rows = sl::get_readings_for_sensor(&span.db, as_uuid(&span.sensor)).await;
    assert_eq!(sensor_rows.len(), 6, "the sensor still owns six readings");
    for (i, row) in sensor_rows.iter().enumerate() {
        assert!(
            row.site_id.is_some() && row.deployment_id.is_some(),
            "row {i} stays attributed, the corrected windows leave no gap"
        );
    }

    let upstream = span.served(&span.site1, &span.parameter).await;
    assert_eq!(
        upstream.len(),
        2,
        "two readings remain upstream after the correction"
    );
    for (i, expected) in [25.0, 45.0].iter().enumerate() {
        assert_close(
            upstream[i],
            *expected,
            &format!("upstream value {i} after the move"),
        );
    }
    let downstream = span.served(&span.site2, &span.parameter).await;
    assert_eq!(
        downstream.len(),
        4,
        "four readings are downstream after the correction"
    );
    for (i, expected) in [65.0, 85.0, 105.0, 125.0].iter().enumerate() {
        assert_close(
            downstream[i],
            *expected,
            &format!("downstream value {i} after the move"),
        );
    }

    let after_up = e2e::hourly_bucket(&span.db, &span.site1, &span.parameter, morning).await;
    assert!(
        after_up.is_some(),
        "the upstream morning bucket survives the move"
    );
    let (mean, count) = after_up.unwrap();
    assert_close(
        mean,
        35.0,
        "the upstream morning mean drops the reading that crossed",
    );
    assert_eq!(
        count, 2,
        "two readings remain in the upstream morning bucket"
    );

    let new_down = e2e::hourly_bucket(&span.db, &span.site2, &span.parameter, morning).await;
    assert!(
        new_down.is_some(),
        "the crossing reading opens a morning bucket at the downstream site"
    );
    let (mean, count) = new_down.unwrap();
    assert_close(
        mean,
        65.0,
        "the downstream morning bucket holds the crossing reading",
    );
    assert_eq!(count, 1, "exactly one reading crossed");

    let untouched = e2e::hourly_bucket(&span.db, &span.site2, &span.parameter, afternoon).await;
    assert!(
        untouched.is_some(),
        "the downstream afternoon bucket survives the move"
    );
    let (mean, count) = untouched.unwrap();
    assert_close(
        mean,
        105.0,
        "the afternoon bucket is outside the correction and does not move",
    );
    assert_eq!(count, 3, "the afternoon bucket still holds three readings");

    let up = span.aggregates(&span.site1, "").await;
    let idx = bucket_index(&up, BUCKET_MORNING);
    assert_close(
        e2e::field_for(&up, &span.parameter, "avg")[idx],
        35.0,
        "the rebalanced upstream average over HTTP",
    );
    let down = span.aggregates(&span.site2, "").await;
    let idx = bucket_index(&down, BUCKET_MORNING);
    assert_close(
        e2e::field_for(&down, &span.parameter, "avg")[idx],
        65.0,
        "the new downstream morning average over HTTP",
    );
    let idx = bucket_index(&down, BUCKET_AFTERNOON);
    assert_close(
        e2e::field_for(&down, &span.parameter, "avg")[idx],
        105.0,
        "the downstream afternoon average over HTTP is unmoved",
    );

    span.assert_control_untouched("after the deployment move")
        .await;
    let ctrl_bucket =
        e2e::hourly_bucket(&span.db, &span.site1, &span.control_parameter, morning).await;
    assert!(
        ctrl_bucket.is_some(),
        "the control bucket survives the move"
    );
    let (mean, count) = ctrl_bucket.unwrap();
    assert_close(
        mean,
        16.0,
        "the co-located control bucket is undisturbed by the move",
    );
    assert_eq!(count, 3, "the control bucket still holds three readings");
}

#[tokio::test]
#[serial]
async fn partial_calibration_window_splits_by_time_not_by_site() {
    if !kc::require_keycloak_or_skip("partial_calibration_window").await {
        return;
    }
    // The span calibration starts mid-morning, so the upstream site holds one reading inside its
    // window and one outside it.
    let span = seed_two_site_span("2025-06-02T10:30:00Z", &PARTIAL_READINGS).await;

    let rows = span.readings().await;
    assert_eq!(
        rows.len(),
        4,
        "the partial-window fixture holds four readings"
    );
    let before = [
        (10.0, &span.base_cal, &span.site1),
        (65.0, &span.span_cal, &span.site1),
        (85.0, &span.span_cal, &span.site2),
        (125.0, &span.span_cal, &span.site2),
    ];
    for (i, (calibrated, calibration, site)) in before.iter().enumerate() {
        assert_eq!(
            rows[i].calibrated_value,
            Some(*calibrated),
            "row {i} before the edit"
        );
        assert_eq!(
            rows[i].calibration_id,
            Some(as_uuid(calibration.as_str())),
            "row {i} resolves by time to its own calibration"
        );
        assert_eq!(
            rows[i].site_id,
            Some(as_uuid(site.as_str())),
            "row {i} site before the edit"
        );
    }

    let window_before = span.calibration_window(&span.span_cal).await;
    assert_eq!(
        window_before["point_count"], 3,
        "the span calibration owns three readings before the edit: {window_before}"
    );
    let base_before = span.calibration_window(&span.base_cal).await;
    assert_eq!(
        base_before["point_count"], 1,
        "the base curve keeps only the reading that predates the span window: {base_before}"
    );

    edit_span_calibration(&span).await;

    let rows = span.readings().await;
    assert_eq!(rows.len(), 4, "the edit neither adds nor drops readings");
    let after = [
        (10.0, 10.0, &span.base_cal, &span.site1, &span.d1),
        (30.0, 91.0, &span.span_cal, &span.site1, &span.d1),
        (40.0, 121.0, &span.span_cal, &span.site2, &span.d2),
        (60.0, 181.0, &span.span_cal, &span.site2, &span.d2),
    ];
    for (i, (raw, calibrated, calibration, site, deployment)) in after.iter().enumerate() {
        assert_eq!(
            rows[i].raw_value, *raw,
            "row {i} raw value survives the edit"
        );
        assert_eq!(
            rows[i].calibrated_value,
            Some(*calibrated),
            "row {i} after the edit, the split is by reading time and not by site"
        );
        assert_eq!(
            rows[i].calibration_id,
            Some(as_uuid(calibration.as_str())),
            "row {i} keeps the calibration its time resolves"
        );
        assert_eq!(
            rows[i].site_id,
            Some(as_uuid(site.as_str())),
            "row {i} site after the edit"
        );
        assert_eq!(
            rows[i].deployment_id,
            Some(as_uuid(deployment.as_str())),
            "row {i} deployment after the edit"
        );
    }

    let upstream = span.served(&span.site1, &span.parameter).await;
    assert_eq!(upstream.len(), 2, "two readings are served upstream");
    assert_close(
        upstream[0],
        10.0,
        "the upstream reading that predates the window keeps the base curve, at the same site as an edited one",
    );
    assert_close(
        upstream[1],
        91.0,
        "the upstream reading inside the window moves",
    );

    let downstream = span.served(&span.site2, &span.parameter).await;
    assert_eq!(downstream.len(), 2, "two readings are served downstream");
    for (i, expected) in [121.0, 181.0].iter().enumerate() {
        assert_close(
            downstream[i],
            *expected,
            &format!("downstream value {i} after the edit"),
        );
    }

    let window_after = span.calibration_window(&span.span_cal).await;
    assert_eq!(
        window_after["point_count"], 3,
        "window membership is unchanged by a coefficient edit: {window_after}"
    );
    assert_close(
        window_after["slope"].as_f64().unwrap_or(f64::NAN),
        3.0,
        "the span calibration reports its new slope",
    );
    let base_after = span.calibration_window(&span.base_cal).await;
    assert_eq!(
        base_after["point_count"], 1,
        "the base curve keeps its single reading: {base_after}"
    );
    assert_close(
        base_after["slope"].as_f64().unwrap_or(f64::NAN),
        1.0,
        "the base curve is untouched by the edit",
    );

    span.assert_control_untouched("after the partial-window edit")
        .await;
}
