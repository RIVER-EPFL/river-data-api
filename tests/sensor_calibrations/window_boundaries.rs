//! Boundaries on the calibration and deployment timelines: what an operator action may move, and
//! what it must leave exactly as the operator entered it.
//!
//! Scenario: an operator repairs calibration coverage for instruments uploaded as history, moves a
//! sensor into a slot another sensor already holds, enters two curves at the same instant, or rolls
//! a deployment back into a slot that has since been refilled.
//!
//! Expected behaviour: a coverage repair never truncates a window a scientist entered, a refused
//! request writes nothing at all, every stored curve owns a window a reading can fall in, and a
//! rollback blocked by the slot constraint says so rather than failing raw.
//!
//! Everything is provisioned over HTTP from the CSV onboarding track, as the roles that own each
//! step: administrator for inventory and for the curves of never-deployed sensors (a calibration on
//! a sensor with no deployment resolves to no project, so a granted member is refused), manager for
//! deployments and for the rollback that deletes one, river for ingestion and the operator data
//! actions.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use serial_test::serial;
use std::time::Duration;
use uuid::Uuid;

use crate::common::e2e;
use crate::common::keycloak as kc;
use crate::common::sensor_lifecycle::wait_for_reprocessing;
use crate::common::tracks;
use crate::common::{
    delete_with_token, get_json_with_token, get_with_token, post_json_parse_with_token,
    post_json_with_token,
};

const WAIT: Duration = Duration::from_secs(30);
const WAIT_SECS: u64 = 30;
const DEPTH: &str = "TrkCsvDepth";
const TURBIDITY: &str = "TrkCsvTurb";

fn ts(s: &str) -> DateTime<Utc> {
    s.parse()
        .unwrap_or_else(|e| panic!("invalid fixture timestamp '{s}': {e}"))
}

fn as_uuid(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap_or_else(|e| panic!("'{s}' is not a uuid: {e}"))
}

/// A window boundary as the API serves it. The key is asserted present before it is read, so a
/// response that never carried the boundary cannot pass as an open-ended window.
fn boundary(entity: &Value, key: &str) -> Option<DateTime<Utc>> {
    assert!(
        entity.get(key).is_some(),
        "the response must carry {key}, null or not: {entity}"
    );
    entity[key].as_str().map(ts)
}

struct Fixture {
    db: sea_orm::DatabaseConnection,
    app: axum::Router,
    admin: String,
    manager: String,
    river: String,
    track: tracks::Track,
}

/// Track A (site plus two catalog parameters, no sensor), plus the two granted members whose roles
/// the flows below need.
async fn onboard() -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;

    kc::ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;

    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    let track = tracks::onboard_csv_track(&app, &admin).await;

    kc::grant_project(&db, &kc::keycloak_user_id("manager1").await, &track.project_id).await;
    kc::grant_project(&db, &kc::keycloak_user_id("river1").await, &track.project_id).await;
    let manager = kc::get_keycloak_jwt("manager1", "manager1").await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;

    Fixture { db, app, admin, manager, river, track }
}

async fn add_sensor(fx: &Fixture, serial: &str) -> String {
    e2e::create_sensor(&fx.app, &fx.admin, fx.track.parameter_id(DEPTH), serial).await
}

/// Enter a windowed curve. `parameter_id` is always sent so the curve joins its parameter's
/// chaining partition.
async fn create_curve(
    fx: &Fixture,
    sensor_id: &str,
    parameter_id: &str,
    slope: f64,
    intercept: f64,
    valid_from: &str,
) -> (u16, Value) {
    post_json_parse_with_token(
        &fx.app,
        "/api/sensor_calibrations",
        &json!({
            "sensor_id": sensor_id,
            "parameter_id": parameter_id,
            "slope": slope,
            "intercept": intercept,
            "valid_from": valid_from,
        }),
        &fx.admin,
    )
    .await
}

async fn add_curve(
    fx: &Fixture,
    sensor_id: &str,
    parameter_id: &str,
    slope: f64,
    intercept: f64,
    valid_from: &str,
) -> String {
    let (status, body) =
        create_curve(fx, sensor_id, parameter_id, slope, intercept, valid_from).await;
    assert_eq!(status, 201, "a curve is entered from {valid_from}: {body}");
    assert!(
        wait_for_reprocessing(&fx.db, as_uuid(sensor_id), WAIT).await,
        "the calibration_create job settles without failing"
    );
    e2e::id_of(&body)
}

/// Every calibration a sensor carries, oldest window first.
async fn curves_of(fx: &Fixture, sensor_id: &str) -> Vec<Value> {
    let filter = e2e::percent_encode(&format!(r#"{{"sensor_id":"{sensor_id}"}}"#));
    let (status, body) = get_json_with_token(
        &fx.app,
        &format!("/api/sensor_calibrations?filter={filter}"),
        &fx.admin,
    )
    .await;
    assert_eq!(status, 200, "list the curves of sensor {sensor_id}: {body}");
    let mut curves = body
        .as_array()
        .unwrap_or_else(|| panic!("the calibration list must be an array: {body}"))
        .clone();
    curves.sort_by_key(|c| boundary(c, "valid_from"));
    curves
}

async fn deployment(fx: &Fixture, deployment_id: &str) -> Value {
    let (status, body) = get_json_with_token(
        &fx.app,
        &format!("/api/sensor_deployments/{deployment_id}"),
        &fx.manager,
    )
    .await;
    assert_eq!(status, 200, "read deployment {deployment_id}: {body}");
    body
}

async fn deployments_of(fx: &Fixture, sensor_id: &str) -> Vec<Value> {
    let filter = e2e::percent_encode(&format!(r#"{{"sensor_id":"{sensor_id}"}}"#));
    let (status, body) = get_json_with_token(
        &fx.app,
        &format!("/api/sensor_deployments?filter={filter}"),
        &fx.manager,
    )
    .await;
    assert_eq!(status, 200, "list the deployments of sensor {sensor_id}: {body}");
    body.as_array()
        .unwrap_or_else(|| panic!("the deployment list must be an array: {body}"))
        .clone()
}

/// Upload instrument history that carries a sensor but no calibration id, the state
/// `backfill_calibrations` exists to repair.
async fn upload_history(fx: &Fixture, rows: &[(&str, &str, f64, &str)]) {
    let readings: Vec<Value> = rows
        .iter()
        .map(|(parameter_id, time, raw_value, sensor_id)| {
            json!({
                "site_id": fx.track.site_id,
                "parameter_id": parameter_id,
                "time": time,
                "raw_value": raw_value,
                "sensor_id": sensor_id,
            })
        })
        .collect();
    let (status, body) = post_json_parse_with_token(
        &fx.app,
        "/api/readings/batch",
        &json!({ "readings": readings }),
        &fx.river,
    )
    .await;
    assert_eq!(status, 200, "river uploads instrument history: {body}");
    assert_eq!(
        body["inserted"].as_u64(),
        Some(rows.len() as u64),
        "every uploaded reading lands: {body}"
    );
}

struct Series {
    times: Vec<DateTime<Utc>>,
    raw: Vec<Option<f64>>,
    calibrated: Vec<Option<f64>>,
}

/// The sensor detail plot's own series, which is what an operator reads after a repair.
async fn series(fx: &Fixture, sensor_id: &str, start: &str, end: &str) -> Series {
    let (status, body) = get_json_with_token(
        &fx.app,
        &format!("/api/sensors/{sensor_id}/readings?start={start}&end={end}"),
        &fx.admin,
    )
    .await;
    assert_eq!(status, 200, "read the series of sensor {sensor_id}: {body}");
    let numbers = |key: &str| {
        body[key]
            .as_array()
            .unwrap_or_else(|| panic!("'{key}' must be an array: {body}"))
            .iter()
            .map(serde_json::Value::as_f64)
            .collect::<Vec<Option<f64>>>()
    };
    let times = body["times"]
        .as_array()
        .unwrap_or_else(|| panic!("'times' must be an array: {body}"))
        .iter()
        .map(|t| ts(t.as_str().unwrap_or_else(|| panic!("a time must be a string: {body}"))))
        .collect();
    Series { times, raw: numbers("raw"), calibrated: numbers("calibrated") }
}

/// `backfill_calibrations` must not insert an identity curve inside an entered
/// calibration's window, which truncates that window and reverts its readings to raw.
#[tokio::test]
#[serial]
async fn backfill_calibrations_leaves_an_entered_curve_covering_its_readings() {
    if !kc::require_keycloak_or_skip("backfill_calibrations_window_boundaries").await {
        return;
    }
    let fx = onboard().await;
    let depth = fx.track.parameter_id(DEPTH).to_string();
    let turbidity = fx.track.parameter_id(TURBIDITY).to_string();

    let covered = add_sensor(&fx, "WB-COVERED-0001").await;
    assert!(
        curves_of(&fx, &covered).await.is_empty(),
        "a sensor entered into inventory carries no calibration until one is entered for it"
    );
    let entered = add_curve(&fx, &covered, &depth, 2.0, 5.0, "2025-01-01T00:00:00Z").await;

    // Control: an instrument whose history genuinely predates its first curve is what the action
    // exists to repair, so a change that simply stops backfilling fails here instead of passing.
    let gapped = add_sensor(&fx, "WB-GAPPED-0001").await;
    let late = add_curve(&fx, &gapped, &turbidity, 3.0, 0.0, "2025-05-01T00:00:00Z").await;

    upload_history(
        &fx,
        &[
            (depth.as_str(), "2025-02-01T12:00:00Z", 10.0, covered.as_str()),
            (depth.as_str(), "2025-03-01T12:00:00Z", 20.0, covered.as_str()),
            (turbidity.as_str(), "2025-04-15T12:00:00Z", 30.0, gapped.as_str()),
        ],
    )
    .await;

    let (status, body) = post_json_parse_with_token(
        &fx.app,
        "/api/actions/backfill_calibrations",
        &json!({ "sensor_ids": [covered, gapped] }),
        &fx.river,
    )
    .await;
    assert_eq!(status, 200, "river repairs calibration coverage for both instruments: {body}");
    let job_id = body["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the action must return a tracked job id: {body}"))
        .to_string();
    assert_eq!(
        e2e::poll_job(&fx.app, &fx.admin, &job_id, WAIT_SECS).await,
        "completed",
        "the backfill_calibrations job settles without failing"
    );

    let curves = curves_of(&fx, &covered).await;
    assert_eq!(
        curves.len(),
        1,
        "readings that already fall inside an entered window need no identity curve: {curves:?}"
    );
    assert_eq!(
        curves[0]["id"].as_str(),
        Some(entered.as_str()),
        "the curve the scientist entered is the one that survives: {curves:?}"
    );
    assert_eq!(
        boundary(&curves[0], "valid_until"),
        None,
        "the entered curve keeps its open end, ie. it still covers its later readings: {curves:?}"
    );

    let covered_series = series(&fx, &covered, "2025-01-01T00:00:00Z", "2025-04-01T00:00:00Z").await;
    assert_eq!(
        covered_series.times,
        vec![ts("2025-02-01T12:00:00Z"), ts("2025-03-01T12:00:00Z")],
        "both uploaded readings are served back"
    );
    assert_eq!(
        covered_series.raw,
        vec![Some(10.0), Some(20.0)],
        "raw values are never rewritten by a repair"
    );
    assert_eq!(
        covered_series.calibrated,
        vec![Some(25.0), Some(45.0)],
        "every reading inside the entered window keeps its coefficients (2 * raw + 5)"
    );

    let gapped_curves = curves_of(&fx, &gapped).await;
    assert_eq!(
        gapped_curves.len(),
        2,
        "history predating every curve is covered by a backfilled identity: {gapped_curves:?}"
    );
    assert_eq!(
        gapped_curves[0]["slope"].as_f64(),
        Some(1.0),
        "the backfilled curve is the identity: {gapped_curves:?}"
    );
    assert_eq!(
        gapped_curves[0]["intercept"].as_f64(),
        Some(0.0),
        "the backfilled curve is the identity: {gapped_curves:?}"
    );
    let identity_from = boundary(&gapped_curves[0], "valid_from")
        .unwrap_or_else(|| panic!("valid_from is never null: {gapped_curves:?}"));
    assert!(
        identity_from <= ts("2025-04-15T12:00:00Z"),
        "the identity reaches back to the earliest uncovered reading: {gapped_curves:?}"
    );
    assert_eq!(
        boundary(&gapped_curves[0], "valid_until"),
        Some(ts("2025-05-01T00:00:00Z")),
        "the identity stops where the entered curve begins: {gapped_curves:?}"
    );
    assert_eq!(
        gapped_curves[1]["id"].as_str(),
        Some(late.as_str()),
        "the entered curve holds the later window: {gapped_curves:?}"
    );
    assert_eq!(
        boundary(&gapped_curves[1], "valid_until"),
        None,
        "the entered curve holds the open end of the timeline: {gapped_curves:?}"
    );
    let gapped_series = series(&fx, &gapped, "2025-04-01T00:00:00Z", "2025-05-01T00:00:00Z").await;
    assert_eq!(
        gapped_series.calibrated,
        vec![Some(30.0)],
        "a reading under a backfilled identity serves its raw value"
    );
}

/// a deployment create refused by the slot pre-check must leave the sensor's current
/// deployment open, ie. the refused request has no side effect.
#[tokio::test]
#[serial]
async fn a_refused_deployment_create_leaves_the_current_deployment_open() {
    if !kc::require_keycloak_or_skip("refused_deployment_create_boundaries").await {
        return;
    }
    let fx = onboard().await;
    let depth = fx.track.parameter_id(DEPTH).to_string();
    let occupied_site = e2e::create_site(
        &fx.app,
        &fx.admin,
        &fx.track.project_id,
        "Track csv downstream",
        "site-csv-down",
    )
    .await;

    let moving = add_sensor(&fx, "WB-MOVING-0001").await;
    let incumbent = add_sensor(&fx, "WB-INCUMBENT-0001").await;
    let moving_deployment = e2e::create_deployment(
        &fx.app,
        &fx.manager,
        &moving,
        &fx.track.site_id,
        &depth,
        "2025-06-02T00:00:00Z",
    )
    .await;
    let incumbent_deployment = e2e::create_deployment(
        &fx.app,
        &fx.manager,
        &incumbent,
        &occupied_site,
        &depth,
        "2025-06-10T00:00:00Z",
    )
    .await;
    assert_eq!(
        boundary(&deployment(&fx, &moving_deployment).await, "deployed_until"),
        None,
        "the moving sensor starts open-ended at its first site"
    );
    assert_eq!(
        boundary(&deployment(&fx, &incumbent_deployment).await, "deployed_until"),
        None,
        "the incumbent holds the target slot open-ended"
    );

    let (status, body) = post_json_with_token(
        &fx.app,
        "/api/sensor_deployments",
        &json!({
            "sensor_id": moving,
            "site_id": occupied_site,
            "parameter_id": depth,
            "deployed_from": "2025-07-01T00:00:00Z",
        }),
        &fx.manager,
    )
    .await;
    assert_eq!(
        status, 400,
        "the target slot is held by another sensor over an overlapping window: {body}"
    );

    assert_eq!(
        boundary(&deployment(&fx, &moving_deployment).await, "deployed_until"),
        None,
        "a refused move must not recall the sensor from the site it is still deployed at"
    );
    assert_eq!(
        boundary(&deployment(&fx, &incumbent_deployment).await, "deployed_until"),
        None,
        "the incumbent's deployment is untouched by the refused request"
    );
    let rows = deployments_of(&fx, &moving).await;
    assert_eq!(rows.len(), 1, "the refused create writes no deployment row: {rows:?}");
}

/// two calibrations entered with the same `valid_from` must not leave one owning an empty
/// `[valid_from, valid_until)` window, ie. a curve that can never apply to a reading.
#[tokio::test]
#[serial]
async fn curves_sharing_a_valid_from_never_collapse_into_an_empty_window() {
    if !kc::require_keycloak_or_skip("duplicate_valid_from_boundaries").await {
        return;
    }
    let fx = onboard().await;
    let depth = fx.track.parameter_id(DEPTH).to_string();
    let sensor = add_sensor(&fx, "WB-DUPLICATE-0001").await;

    let first = add_curve(&fx, &sensor, &depth, 2.0, 0.0, "2025-04-01T00:00:00Z").await;
    let (status, body) = create_curve(&fx, &sensor, &depth, 3.0, 1.0, "2025-04-01T00:00:00Z").await;

    if (400..500).contains(&status) {
        let curves = curves_of(&fx, &sensor).await;
        assert_eq!(
            curves.len(),
            1,
            "a refused duplicate leaves the timeline as it was: {curves:?}"
        );
        assert_eq!(
            curves[0]["id"].as_str(),
            Some(first.as_str()),
            "the surviving curve is the one entered first: {curves:?}"
        );
        assert_eq!(
            boundary(&curves[0], "valid_until"),
            None,
            "the surviving curve keeps its open window: {curves:?}"
        );
        return;
    }

    assert_eq!(
        status, 201,
        "a duplicate valid_from is either refused or accepted, nothing else: {body}"
    );
    let curves = curves_of(&fx, &sensor).await;
    assert_eq!(
        curves.len(),
        2,
        "both curves are stored when the duplicate is accepted: {curves:?}"
    );
    for curve in &curves {
        let from = boundary(curve, "valid_from")
            .unwrap_or_else(|| panic!("valid_from is never null: {curve}"));
        assert!(
            boundary(curve, "valid_until").is_none_or(|until| until > from),
            "a stored curve must own a window a reading can fall in, this one applies to \
             nothing: {curve}"
        );
    }
}

/// rolling a deployment back into a slot that has since been refilled must report a
/// conflict, not fail on the raw slot-exclusion constraint.
#[tokio::test]
#[serial]
async fn rolling_back_into_a_refilled_slot_reports_a_conflict() {
    if !kc::require_keycloak_or_skip("rollback_into_refilled_slot").await {
        return;
    }
    let fx = onboard().await;
    let depth = fx.track.parameter_id(DEPTH).to_string();
    let second_site = e2e::create_site(
        &fx.app,
        &fx.admin,
        &fx.track.project_id,
        "Track csv upstream",
        "site-csv-up",
    )
    .await;

    let moving = add_sensor(&fx, "WB-ROLLBACK-0001").await;
    let replacement = add_sensor(&fx, "WB-REPLACEMENT-0001").await;

    let first = e2e::create_deployment(
        &fx.app,
        &fx.manager,
        &moving,
        &fx.track.site_id,
        &depth,
        "2025-06-02T00:00:00Z",
    )
    .await;
    let moved = e2e::create_deployment(
        &fx.app,
        &fx.manager,
        &moving,
        &second_site,
        &depth,
        "2025-07-01T00:00:00Z",
    )
    .await;
    assert_eq!(
        boundary(&deployment(&fx, &first).await, "deployed_until"),
        Some(ts("2025-07-01T00:00:00Z")),
        "moving the sensor recalls it from its first site"
    );
    let refill = e2e::create_deployment(
        &fx.app,
        &fx.manager,
        &replacement,
        &fx.track.site_id,
        &depth,
        "2025-08-01T00:00:00Z",
    )
    .await;

    let (status, body) = post_json_with_token(
        &fx.app,
        "/api/actions/rollback_deployment",
        &json!({ "deployment_id": moved }),
        &fx.manager,
    )
    .await;
    assert_eq!(
        status, 409,
        "the vacated slot now holds another sensor, so the rollback reports a conflict: {body}"
    );
    assert!(
        !body.contains("excl_deployment_site_param_slot"),
        "the operator is told which deployment blocks the rollback, not the constraint name: \
         {body}"
    );

    assert_eq!(
        deployment(&fx, &moved).await["id"].as_str(),
        Some(moved.as_str()),
        "a refused rollback deletes nothing"
    );
    assert_eq!(
        boundary(&deployment(&fx, &first).await, "deployed_until"),
        Some(ts("2025-07-01T00:00:00Z")),
        "the predecessor stays closed while the rollback is refused"
    );
    assert_eq!(
        boundary(&deployment(&fx, &refill).await, "deployed_until"),
        None,
        "the replacement keeps the slot it was deployed into"
    );

    // Control: with the slot free again the same rollback is not blocked, so the conflict above is
    // about the refilled slot and not about rollback being broken.
    let (status, body) = delete_with_token(
        &fx.app,
        &format!("/api/sensor_deployments/{refill}"),
        &fx.manager,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "the manager removes the replacement's deployment ({status}): {body}"
    );

    let (status, body) = post_json_parse_with_token(
        &fx.app,
        "/api/actions/rollback_deployment",
        &json!({ "deployment_id": moved }),
        &fx.manager,
    )
    .await;
    assert_eq!(status, 200, "with the slot free the same rollback completes: {body}");
    assert_eq!(
        body["previous_deployment_id"].as_str(),
        Some(first.as_str()),
        "the rollback reopens the deployment its target superseded: {body}"
    );
    assert_eq!(
        boundary(&deployment(&fx, &first).await, "deployed_until"),
        None,
        "the predecessor is open again once the rollback completes"
    );
    let (status, body) = get_with_token(
        &fx.app,
        &format!("/api/sensor_deployments/{moved}"),
        &fx.manager,
    )
    .await;
    assert_eq!(status, 404, "the rolled-back deployment is gone: {body}");
}
