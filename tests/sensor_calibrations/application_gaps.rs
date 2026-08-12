//! Where a calibration is resolved but never applied, and where a window an operator sets is not
//! the window that is stored.
//!
//! Scenario: an operator enters a curve for a deployed instrument, then data keeps arriving through
//! ingest, grab entry and stream import.
//! Expected behaviour: every reading a curve covers is served through that curve without an
//! operator having to ask for a reprocess, the curve a reading is stamped with is the one whose
//! parameter and window cover it, and an explicit end date the operator sets is the end date
//! stored.
//!
//! Each test names the finding in `docs/defect-findings.md` that it proves.

use std::time::Duration;

use axum::Router;
use chrono::{DateTime, Utc};
use sea_orm::DatabaseConnection;
use serde_json::{Value, json};
use serial_test::serial;
use uuid::Uuid;

use crate::common::e2e;
use crate::common::keycloak as kc;
use crate::common::sensor_lifecycle as sl;
use crate::common::tracks;
use crate::common::{
    get_json_with_token, post_json_parse_with_token, post_json_with_token, put_json_with_token,
};

const WAIT: Duration = Duration::from_secs(30);
const JOB_TIMEOUT_SECS: u64 = 30;

/// Track B sits on 2025-06-02. Every fixture timestamp below stays in that week, ie. in the past,
/// because the aggregate refresh window is `[since, NOW()]`.
const CURVE_FROM: &str = "2025-06-01T00:00:00Z";
const LATER_CURVE_FROM: &str = "2025-07-01T00:00:00Z";
const RETIRED_AT: &str = "2025-06-15T00:00:00Z";

const T_BEFORE_CURVE_ENTRY: &str = "2025-06-02T10:00:00Z";
const T_AFTER_CURVE_ENTRY: &str = "2025-06-02T12:00:00Z";
const BUCKET_BEFORE_CURVE_ENTRY: &str = "2025-06-02T10:00:00Z";
const BUCKET_AFTER_CURVE_ENTRY: &str = "2025-06-02T12:00:00Z";

const GRAB_TIME: &str = "2025-06-02T09:00:00Z";
const CONTINUOUS_TIME: &str = "2025-06-02T09:30:00Z";

const SECOND_CURVE_FROM: &str = "2025-06-05T00:00:00Z";
const MIXED_PARAMETER_TIME: &str = "2025-06-10T09:00:00Z";

const EARLY_READING: &str = "2025-06-02T00:00:00Z";
const LATE_READING: &str = "2025-06-12T00:00:00Z";
const LATE_CURVE_FROM: &str = "2025-06-10T00:00:00Z";

const WINDOW_START: &str = "2025-06-01T00:00:00Z";
const WINDOW_END: &str = "2025-06-15T00:00:00Z";

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

fn str_field(value: &Value, key: &str) -> String {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("'{key}' is not a string: {value}"))
        .to_string()
}

/// A timestamp field, `None` when the field is null or absent.
fn time_field(value: &Value, key: &str) -> Option<DateTime<Utc>> {
    let raw = value[key].as_str()?;
    Some(
        raw.parse::<DateTime<Utc>>()
            .unwrap_or_else(|e| panic!("'{key}' is not a timestamp ('{raw}'): {e}")),
    )
}

fn number_array(value: &Value, key: &str) -> Vec<f64> {
    value[key]
        .as_array()
        .unwrap_or_else(|| panic!("'{key}' is not an array: {value}"))
        .iter()
        .map(|v| v.as_f64().unwrap_or(f64::NAN))
        .collect()
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

struct Fixture {
    app: Router,
    db: DatabaseConnection,
    jwt: String,
    site: String,
    parameter: String,
    site_parameter: String,
    sensor: String,
    stream: String,
}

/// Track B: a project, a site, a parameter, an instrument, its deployment and its stream, all
/// provisioned over HTTP as a real Keycloak administrator. The stream is left unlinked and unpaired
/// so each test performs the dashboard steps its scenario needs.
async fn onboard() -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_sensor_flow_track(&app, &jwt).await;
    Fixture {
        site: track.site_id.clone(),
        parameter: track.parameter_id("TrkFlowDO").to_string(),
        site_parameter: track.site_parameter_ids[0].clone(),
        sensor: track
            .sensor_id
            .clone()
            .expect("track B provisions a sensor"),
        stream: track.stream_ids[0].clone(),
        app,
        db,
        jwt,
    }
}

impl Fixture {
    /// Attach a registered stream to the instrument that feeds it.
    async fn link(&self, stream: &str) {
        e2e::link_stream_sensor(&self.app, &self.jwt, stream, &self.sensor).await;
    }

    async fn pair(&self, stream: &str, site_parameter: &str) {
        let (status, body) = post_json_with_token(
            &self.app,
            &format!("/api/streams/{stream}/pair"),
            &json!({ "site_parameter_id": site_parameter }),
            &self.jwt,
        )
        .await;
        assert!(
            (200..300).contains(&status),
            "pair stream {stream} ({status}): {body}"
        );
    }

    async fn register(&self, source_key: &str) -> String {
        let (status, body) = post_json_parse_with_token(
            &self.app,
            "/api/streams/register",
            &json!({
                "source_system": "appgap",
                "source_key": source_key,
                "source_name": format!("Application gap {source_key}"),
            }),
            &self.jwt,
        )
        .await;
        assert!(
            (200..300).contains(&status),
            "register stream {source_key} ({status}): {body}"
        );
        e2e::id_of(&body)
    }

    async fn ingest(&self, stream: &str, readings: &[(&str, f64)]) {
        let payload = json!({
            "stream_id": stream,
            "readings": readings
                .iter()
                .map(|(t, v)| json!({ "time": t, "raw_value": v }))
                .collect::<Vec<_>>(),
        });
        let (status, body) =
            post_json_parse_with_token(&self.app, "/api/ingest", &payload, &self.jwt).await;
        assert_eq!(status, 200, "ingest into {stream} ({status}): {body}");
        assert_eq!(
            body["inserted"],
            readings.len(),
            "every ingested reading lands: {body}"
        );
    }

    async fn create_curve(
        &self,
        parameter: &str,
        slope: f64,
        intercept: f64,
        valid_from: &str,
    ) -> String {
        let (status, body) = post_json_parse_with_token(
            &self.app,
            "/api/sensor_calibrations",
            &json!({
                "sensor_id": self.sensor,
                "parameter_id": parameter,
                "slope": slope,
                "intercept": intercept,
                "valid_from": valid_from,
            }),
            &self.jwt,
        )
        .await;
        assert!(
            (200..300).contains(&status),
            "create curve {slope}x+{intercept} from {valid_from} ({status}): {body}"
        );
        e2e::id_of(&body)
    }

    async fn calibration(&self, calibration: &str) -> Value {
        let (status, body) = get_json_with_token(
            &self.app,
            &format!("/api/sensor_calibrations/{calibration}"),
            &self.jwt,
        )
        .await;
        assert_eq!(status, 200, "read calibration ({status}): {body}");
        body
    }

    /// Values served at the site for one parameter, as the dashboard plot requests them.
    async fn served(&self, parameter: &str, extra: &str) -> Vec<f64> {
        let (status, body) = get_json_with_token(
            &self.app,
            &format!(
                "/api/sites/{}/readings?start={WINDOW_START}&end={WINDOW_END}&parameter_ids={parameter}{extra}",
                self.site
            ),
            &self.jwt,
        )
        .await;
        assert_eq!(status, 200, "served readings ({status}): {body}");
        e2e::values_for(&body, parameter)
    }

    async fn sensor_series(&self) -> Value {
        let (status, body) = get_json_with_token(
            &self.app,
            &format!(
                "/api/sensors/{}/readings?start={WINDOW_START}&end={WINDOW_END}",
                self.sensor
            ),
            &self.jwt,
        )
        .await;
        assert_eq!(status, 200, "sensor series ({status}): {body}");
        body
    }

    async fn aggregates(&self) -> Value {
        let (status, body) = get_json_with_token(
            &self.app,
            &format!(
                "/api/sites/{}/aggregates/hourly?start={WINDOW_START}&end={WINDOW_END}",
                self.site
            ),
            &self.jwt,
        )
        .await;
        assert_eq!(status, 200, "hourly aggregates ({status}): {body}");
        body
    }

    async fn refresh_aggregates(&self) {
        let (status, body) = post_json_parse_with_token(
            &self.app,
            "/api/actions/refresh_aggregates",
            &json!({ "full": true }),
            &self.jwt,
        )
        .await;
        assert_eq!(status, 200, "refresh aggregates ({status}): {body}");
        let job_id = str_field(&body, "job_id");
        let final_status = e2e::poll_job(&self.app, &self.jwt, &job_id, JOB_TIMEOUT_SECS).await;
        assert_eq!(
            final_status, "completed",
            "the refresh job completes: {body}"
        );
    }

    async fn reprocess(&self) {
        let (status, body) = post_json_parse_with_token(
            &self.app,
            "/api/actions/reprocess",
            &json!({ "sensor_id": self.sensor }),
            &self.jwt,
        )
        .await;
        assert_eq!(status, 200, "operator reprocess ({status}): {body}");
        let job_id = str_field(&body, "job_id");
        let final_status = e2e::poll_job(&self.app, &self.jwt, &job_id, JOB_TIMEOUT_SECS).await;
        assert_eq!(
            final_status, "completed",
            "the reprocess job completes: {body}"
        );
    }
}

// RD-023: a reading ingested while a curve is in force is served through that curve, in the
// readings response, the sensor plot and the hourly rollup, with no operator reprocess in between.
#[tokio::test]
#[serial]
async fn routine_ingest_serves_the_active_calibration() {
    if !kc::require_keycloak_or_skip("routine_ingest_serves_the_active_calibration").await {
        return;
    }
    let f = onboard().await;
    f.link(&f.stream).await;
    f.pair(&f.stream, &f.site_parameter).await;

    f.ingest(&f.stream, &[(T_BEFORE_CURVE_ENTRY, 10.0)]).await;

    let curve = f.create_curve(&f.parameter, 2.0, 5.0, CURVE_FROM).await;
    assert!(
        sl::wait_for_reprocessing(&f.db, as_uuid(&f.sensor), WAIT).await,
        "entering a curve enqueues reprocessing for the instrument and it completes"
    );

    let before = f.served(&f.parameter, "").await;
    assert_eq!(
        before.len(),
        1,
        "the reading taken before the curve was entered is served: {before:?}"
    );
    assert_close(
        before[0],
        25.0,
        "the curve's own job rewrites the reading it covers (2*10+5)",
    );

    f.ingest(&f.stream, &[(T_AFTER_CURVE_ENTRY, 10.0)]).await;

    let after = f.served(&f.parameter, "").await;
    assert_eq!(after.len(), 2, "both readings are served: {after:?}");
    assert_close(after[0], 25.0, "the earlier reading keeps its correction");
    assert_close(
        after[1],
        25.0,
        "a reading ingested while the curve is in force is served corrected (2*10+5)",
    );

    let rows = sl::get_readings(&f.db, as_uuid(&f.stream)).await;
    assert_eq!(rows.len(), 2, "the stream holds both readings");
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(
            row.calibration_id,
            Some(as_uuid(&curve)),
            "row {i} is attributed to the 2x+5 curve"
        );
        assert_eq!(row.raw_value, 10.0, "row {i} keeps its raw value");
        assert_eq!(
            row.calibrated_value,
            Some(25.0),
            "row {i} stores the value the curve it names produces"
        );
    }

    let series = f.sensor_series().await;
    assert_eq!(
        number_array(&series, "raw"),
        vec![10.0, 10.0],
        "the sensor plot keeps both raw values: {series}"
    );
    assert_eq!(
        number_array(&series, "calibrated"),
        vec![25.0, 25.0],
        "the sensor plot's calibrated series is corrected: {series}"
    );

    f.refresh_aggregates().await;
    let agg = f.aggregates().await;
    let avg = e2e::field_for(&agg, &f.parameter, "avg");
    let early = bucket_index(&agg, BUCKET_BEFORE_CURVE_ENTRY);
    let late = bucket_index(&agg, BUCKET_AFTER_CURVE_ENTRY);
    assert_close(
        avg[early],
        25.0,
        "the bucket holding the earlier reading rolls up the corrected value",
    );
    assert_close(
        avg[late],
        25.0,
        "the bucket holding the reading ingested under the curve rolls up the corrected value",
    );
}

// RD-025: an explicit end date an operator sets on a curve is stored and read back, on the update
// response, on the entity and in the window editor's own read path.
#[tokio::test]
#[serial]
async fn operator_can_set_an_explicit_calibration_window() {
    if !kc::require_keycloak_or_skip("operator_can_set_an_explicit_calibration_window").await {
        return;
    }
    let f = onboard().await;
    let curve = f.create_curve(&f.parameter, 2.0, 5.0, CURVE_FROM).await;

    // `notes` travels in the same request as a field that is known to be honoured, so the request
    // cannot be dismissed as a no-op edit: whatever happens to `valid_until` happens to a PUT the
    // server did accept and apply.
    let (status, body) = put_json_with_token(
        &f.app,
        &format!("/api/sensor_calibrations/{curve}"),
        &json!({ "valid_until": RETIRED_AT, "notes": "retired after the June service" }),
        &f.jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "an operator retires a curve by giving it an end date ({status}): {body}"
    );
    let updated: Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("update response is not JSON: {e}\nBody: {body}"));
    assert_eq!(
        updated["notes"].as_str(),
        Some("retired after the June service"),
        "the edit was applied: {updated}"
    );
    assert_eq!(
        time_field(&updated, "valid_until"),
        Some(sl::dt(RETIRED_AT)),
        "the same update carries back the end date that was sent: {updated}"
    );
    assert_eq!(
        time_field(&updated, "valid_from"),
        Some(sl::dt(CURVE_FROM)),
        "the window start is untouched: {updated}"
    );
    assert_eq!(
        updated["slope"].as_f64(),
        Some(2.0),
        "the slope is untouched: {updated}"
    );
    assert_eq!(
        updated["intercept"].as_f64(),
        Some(5.0),
        "the intercept is untouched: {updated}"
    );

    let stored = f.calibration(&curve).await;
    assert_eq!(
        time_field(&stored, "valid_until"),
        Some(sl::dt(RETIRED_AT)),
        "the stored curve keeps the operator's end date: {stored}"
    );

    let (status, window) = get_json_with_token(
        &f.app,
        &format!("/api/sensor_calibrations/{curve}/window"),
        &f.jwt,
    )
    .await;
    assert_eq!(status, 200, "calibration window ({status}): {window}");
    assert_eq!(
        time_field(&window, "valid_until"),
        Some(sl::dt(RETIRED_AT)),
        "the window editor reads back the operator's end date: {window}"
    );

    let later = f
        .create_curve(&f.parameter, 3.0, 0.0, LATER_CURVE_FROM)
        .await;
    let stored = f.calibration(&curve).await;
    assert_eq!(
        time_field(&stored, "valid_until"),
        Some(sl::dt(RETIRED_AT)),
        "a curve entered later does not extend the retired window: {stored}"
    );
    let stored_later = f.calibration(&later).await;
    assert_eq!(
        time_field(&stored_later, "valid_until"),
        None,
        "the newest curve stays open-ended: {stored_later}"
    );
}

// RD-026: ingest stamps the curve covering the reading's own parameter, and a later reprocess
// resolves the same curve, so provenance does not change under a rerun.
#[tokio::test]
#[serial]
async fn ingest_stamps_the_curve_for_the_readings_own_parameter() {
    if !kc::require_keycloak_or_skip("ingest_stamps_the_curve_for_the_readings_own_parameter").await
    {
        return;
    }
    let f = onboard().await;
    f.link(&f.stream).await;
    f.pair(&f.stream, &f.site_parameter).await;

    let second_parameter = e2e::create_parameter(
        &f.app,
        &f.jwt,
        "AppGapCond",
        "Application gap conductivity",
        "uS/cm",
    )
    .await;
    let second_slot =
        e2e::assign_site_parameter_minimal(&f.app, &f.jwt, &f.site, &second_parameter).await;
    let second_stream = f.register("appgap-cond").await;
    f.link(&second_stream).await;
    f.pair(&second_stream, &second_slot).await;

    // One instrument, one channel per parameter: each channel's curve is open-ended, so only the
    // parameter tells them apart.
    let first_curve = f.create_curve(&f.parameter, 2.0, 5.0, CURVE_FROM).await;
    let second_curve = f
        .create_curve(&second_parameter, 3.0, 0.0, SECOND_CURVE_FROM)
        .await;
    assert!(
        sl::wait_for_reprocessing(&f.db, as_uuid(&f.sensor), WAIT).await,
        "both curve creates enqueue reprocessing and it completes"
    );

    f.ingest(&f.stream, &[(MIXED_PARAMETER_TIME, 10.0)]).await;
    f.ingest(&second_stream, &[(MIXED_PARAMETER_TIME, 10.0)])
        .await;

    let second_rows = sl::get_readings(&f.db, as_uuid(&second_stream)).await;
    assert_eq!(
        second_rows.len(),
        1,
        "the second channel's stream holds its reading"
    );
    assert_eq!(
        second_rows[0].calibration_id,
        Some(as_uuid(&second_curve)),
        "ingest stamps the curve of the reading's own parameter"
    );

    let first_rows = sl::get_readings(&f.db, as_uuid(&f.stream)).await;
    assert_eq!(
        first_rows.len(),
        1,
        "the first channel's stream holds its reading"
    );
    assert_eq!(
        first_rows[0].calibration_id,
        Some(as_uuid(&first_curve)),
        "the first channel's reading is stamped with its own curve"
    );

    f.reprocess().await;

    let second_rows = sl::get_readings(&f.db, as_uuid(&second_stream)).await;
    assert_eq!(
        second_rows[0].calibration_id,
        Some(as_uuid(&second_curve)),
        "reprocess resolves the same curve ingest did"
    );
    assert_eq!(
        second_rows[0].calibrated_value,
        Some(30.0),
        "the reading carries its own channel's correction (3*10+0)"
    );

    let first_rows = sl::get_readings(&f.db, as_uuid(&f.stream)).await;
    assert_eq!(
        first_rows[0].calibration_id,
        Some(as_uuid(&first_curve)),
        "the first channel keeps its curve through the reprocess"
    );
    assert_eq!(
        first_rows[0].calibrated_value,
        Some(25.0),
        "the first channel carries its own correction (2*10+5)"
    );
}

// RD-027: a grab whose instrument resolves a windowed curve is served through that curve, and an
// operator reprocess keeps it corrected rather than skipping it.
#[tokio::test]
#[serial]
async fn grab_readings_receive_their_resolved_curve() {
    if !kc::require_keycloak_or_skip("grab_readings_receive_their_resolved_curve").await {
        return;
    }
    let f = onboard().await;
    f.link(&f.stream).await;
    f.pair(&f.stream, &f.site_parameter).await;

    let curve = f.create_curve(&f.parameter, 2.0, 5.0, CURVE_FROM).await;
    assert!(
        sl::wait_for_reprocessing(&f.db, as_uuid(&f.sensor), WAIT).await,
        "entering the curve enqueues reprocessing for the instrument and it completes"
    );

    // The grab names the instrument but no curve, so the curve is resolved from the instrument's
    // windows, the case the lab-grab model leaves to window resolution.
    let (status, body) = post_json_with_token(
        &f.app,
        "/api/grab_samples",
        &json!({
            "site_id": f.site,
            "created_by": "audit",
            "readings": [{
                "parameter_id": f.parameter,
                "sensor_id": f.sensor,
                "value": 10.0,
                "time": GRAB_TIME,
            }],
        }),
        &f.jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "grab entry against the deployed instrument ({status}): {body}"
    );

    f.ingest(&f.stream, &[(CONTINUOUS_TIME, 10.0)]).await;

    let spot = f.served(&f.parameter, "&measurement_type=spot").await;
    assert_eq!(spot.len(), 1, "the grab is the only spot point: {spot:?}");
    assert_close(
        spot[0],
        25.0,
        "the grab is served through the curve its instrument resolves (2*10+5)",
    );

    let rows = sl::get_readings_for_sensor(&f.db, as_uuid(&f.sensor)).await;
    assert_eq!(
        rows.len(),
        2,
        "the instrument owns the grab and the continuous reading"
    );
    assert_eq!(
        rows[0].calibration_id,
        Some(as_uuid(&curve)),
        "the grab is attributed to the curve covering it"
    );
    assert_eq!(
        rows[0].calibrated_value,
        Some(25.0),
        "an attributed grab stores the corrected value, not a bare curve id"
    );

    f.reprocess().await;

    let rows = sl::get_readings_for_sensor(&f.db, as_uuid(&f.sensor)).await;
    assert_eq!(
        rows[0].calibrated_value,
        Some(25.0),
        "the operator's reprocess reaches the grab"
    );
    assert_eq!(
        rows[1].calibrated_value,
        Some(25.0),
        "the continuous reading in the same window is corrected by the same curve"
    );

    let spot = f.served(&f.parameter, "&measurement_type=spot").await;
    assert_close(
        spot[0],
        25.0,
        "the served grab stays corrected after the reprocess",
    );
    let continuous = f.served(&f.parameter, "&measurement_type=continuous").await;
    assert_eq!(
        continuous.len(),
        1,
        "the continuous control is the only continuous point: {continuous:?}"
    );
    assert_close(
        continuous[0],
        25.0,
        "the continuous control is corrected by the same curve after the reprocess",
    );
}

// RD-029: importing a stream's instrument stamps each reading with the curve whose window covers
// it, and an identity curve minted by the import covers the history it is stamped on.
#[tokio::test]
#[serial]
async fn stream_import_attributes_each_reading_to_its_covering_curve() {
    if !kc::require_keycloak_or_skip("stream_import_attributes_each_reading_to_its_covering_curve")
        .await
    {
        return;
    }
    let f = onboard().await;
    f.ingest(&f.stream, &[(EARLY_READING, 10.0), (LATE_READING, 10.0)])
        .await;

    let early_curve = f.create_curve(&f.parameter, 2.0, 5.0, CURVE_FROM).await;
    let late_curve = f
        .create_curve(&f.parameter, 3.0, 0.0, LATE_CURVE_FROM)
        .await;
    assert!(
        sl::wait_for_reprocessing(&f.db, as_uuid(&f.sensor), WAIT).await,
        "the curve creates settle before the import"
    );

    f.link(&f.stream).await;
    let (status, imported) = post_json_parse_with_token(
        &f.app,
        &format!("/api/streams/{}/import", f.stream),
        &json!({}),
        &f.jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "import the stream's instrument ({status}): {imported}"
    );
    assert_eq!(
        str_field(&imported, "sensor_id"),
        f.sensor,
        "import reuses the instrument the stream is linked to: {imported}"
    );
    assert_eq!(
        imported["attributed"], 2,
        "both unattributed readings are stamped: {imported}"
    );

    let rows = sl::get_readings(&f.db, as_uuid(&f.stream)).await;
    assert_eq!(rows.len(), 2, "the stream holds both readings");
    assert_eq!(
        rows[0].calibration_id,
        Some(as_uuid(&early_curve)),
        "the June 2 reading carries the curve whose window covers it"
    );
    assert_eq!(
        rows[1].calibration_id,
        Some(as_uuid(&late_curve)),
        "the June 12 reading carries the later curve, which does cover it"
    );
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(
            row.sensor_id,
            Some(as_uuid(&f.sensor)),
            "row {i} is attributed to the imported instrument"
        );
    }

    // A stream with no instrument mints one, and mints its identity curve with it.
    let fresh_stream = f.register("appgap-import-fresh").await;
    f.ingest(&fresh_stream, &[(EARLY_READING, 20.0)]).await;
    let (status, imported) = post_json_parse_with_token(
        &f.app,
        &format!("/api/streams/{fresh_stream}/import"),
        &json!({}),
        &f.jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "import a stream with no linked instrument ({status}): {imported}"
    );
    assert_ne!(
        str_field(&imported, "sensor_id"),
        f.sensor,
        "a stream with no instrument mints its own: {imported}"
    );
    assert_eq!(
        imported["attributed"], 1,
        "the fresh stream's reading is stamped: {imported}"
    );

    let fresh_rows = sl::get_readings(&f.db, as_uuid(&fresh_stream)).await;
    assert_eq!(fresh_rows.len(), 1, "the fresh stream holds its reading");
    assert!(
        fresh_rows[0].calibration_id.is_some(),
        "import stamps a curve on the reading it attributes"
    );
    let minted_curve = fresh_rows[0].calibration_id.unwrap().to_string();
    let curve = f.calibration(&minted_curve).await;
    let valid_from = time_field(&curve, "valid_from");
    assert!(
        valid_from.is_some(),
        "the minted curve has a window start: {curve}"
    );
    assert!(
        valid_from.unwrap() <= sl::dt(EARLY_READING),
        "the curve minted on import covers the reading it is stamped on: {curve}"
    );
}
