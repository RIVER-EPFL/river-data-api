//! What a calibration window is, at its edges: the instant it opens, a duplicate opening instant,
//! an out-of-order insert, and the leading region before a sensor's first curve.
//!
//! Run: cargo test --test sensor_calibrations -- --test-threads=1

use crate::common::e2e;
use crate::common::sensor_lifecycle::*;
use crate::common::*;
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use std::time::Duration;
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(30);

struct Curve {
    id: Uuid,
    slope: f64,
    valid_from: DateTime<Utc>,
    valid_until: Option<DateTime<Utc>>,
}

async fn exec_sql(db: &DatabaseConnection, sql: &str) {
    db.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// The sensor's windowed curves, oldest first.
async fn curves_of(db: &DatabaseConnection, sensor_id: Uuid) -> Vec<Curve> {
    db.query_all(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT id, slope, valid_from, valid_until FROM sensor_calibrations \
             WHERE sensor_id = '{sensor_id}' ORDER BY valid_from, id"
        ),
    ))
    .await
    .expect("query sensor_calibrations")
    .iter()
    .map(|row| {
        let from: DateTime<chrono::FixedOffset> =
            row.try_get("", "valid_from").expect("valid_from");
        let until: Option<DateTime<chrono::FixedOffset>> =
            row.try_get("", "valid_until").expect("valid_until");
        Curve {
            id: row.try_get("", "id").expect("id"),
            slope: row.try_get("", "slope").expect("slope"),
            valid_from: from.with_timezone(&Utc),
            valid_until: until.map(|t| t.with_timezone(&Utc)),
        }
    })
    .collect()
}

async fn create_curve(
    app: &axum::Router,
    token: &str,
    sensor_id: Uuid,
    slope: f64,
    valid_from: &str,
) -> (u16, serde_json::Value) {
    post_json_parse_with_token(
        app,
        "/api/sensor_calibrations",
        &json!({
            "sensor_id": sensor_id,
            "slope": slope,
            "intercept": 0.0,
            "valid_from": valid_from,
        }),
        token,
    )
    .await
}

/// Two windowed curves opening at the same instant leave one of them with a zero-width window: the
/// chain runs each curve's end down to the next curve's start, so whichever loses the tie applies to
/// nothing while remaining visible in the editor.
#[tokio::test]
#[serial]
async fn two_curves_sharing_an_opening_instant_are_refused() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    let sensor = create_sensor(&db, "window-duplicate", GLOBAL_PARAM_TEMP_ID).await;

    let (status, first) = create_curve(&app, &token, sensor.id, 2.0, "2025-05-01T00:00:00Z").await;
    assert_eq!(
        status, 201,
        "the first curve at that instant is recorded: {first}"
    );

    let (status, second) = create_curve(&app, &token, sensor.id, 3.0, "2025-05-01T00:00:00Z").await;
    assert_eq!(
        status, 400,
        "a second curve at the same instant is refused: {second}"
    );

    let curves = curves_of(&db, sensor.id).await;
    assert_eq!(
        curves.len(),
        2,
        "the bench base plus the one accepted curve, nothing more"
    );
    for curve in &curves {
        assert_ne!(
            curve.valid_until,
            Some(curve.valid_from),
            "no curve is left with an empty window"
        );
    }

    // One second later is a different window and is accepted, so the refusal is about the collision
    // and not about the sensor already holding a curve.
    let (status, third) = create_curve(&app, &token, sensor.id, 3.0, "2025-05-01T00:00:01Z").await;
    assert_eq!(status, 201, "a distinct instant is accepted: {third}");
}

/// The window is half-open: `[valid_from, next_valid_from)`. The instant a curve opens belongs to
/// that curve, and the instant before it belongs to the previous one.
#[tokio::test]
#[serial]
async fn the_opening_instant_belongs_to_the_curve_it_opens() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    let sensor = create_sensor(&db, "window-boundary", GLOBAL_PARAM_TEMP_ID).await;
    let deployment = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2024-01-01T00:00:00Z")).await;
    let stream = create_paired_stream(&db, "window-boundary", PARAM_S1_TEMP_ID).await;

    let just_before = dt("2025-01-31T23:59:59.999999Z");
    let exactly_at = dt("2025-02-01T00:00:00Z");
    insert_readings(
        &db,
        stream,
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        sensor.id,
        sensor.base_calibration_id,
        deployment,
        1.0,
        0.0,
        &[(just_before, 10.0), (exactly_at, 10.0)],
    )
    .await;

    for (slope, valid_from) in [(2.0, "2025-01-01T00:00:00Z"), (3.0, "2025-02-01T00:00:00Z")] {
        let (status, body) = create_curve(&app, &token, sensor.id, slope, valid_from).await;
        assert_eq!(status, 201, "record the curve at {valid_from}: {body}");
        assert!(
            wait_for_reprocessing(&db, sensor.id, WAIT).await,
            "the curve at {valid_from} reprocesses the history it covers"
        );
    }

    let curves = curves_of(&db, sensor.id).await;
    let earlier = curves
        .iter()
        .find(|c| (c.slope - 2.0).abs() < f64::EPSILON)
        .expect("the 2x curve");
    let later = curves
        .iter()
        .find(|c| (c.slope - 3.0).abs() < f64::EPSILON)
        .expect("the 3x curve");

    let rows = get_readings(&db, stream).await;
    let before = rows
        .iter()
        .find(|r| r.time == just_before)
        .expect("the reading a microsecond before the boundary");
    let at = rows
        .iter()
        .find(|r| r.time == exactly_at)
        .expect("the reading exactly on the boundary");

    assert_eq!(
        before.calibration_id,
        Some(earlier.id),
        "a microsecond before the boundary is still the earlier curve"
    );
    assert_eq!(before.calibrated_value, Some(20.0), "and carries 2*10");
    assert_eq!(
        at.calibration_id,
        Some(later.id),
        "the boundary instant itself opens the later curve"
    );
    assert_eq!(at.calibrated_value, Some(30.0), "and carries 3*10");
}

/// Windows chain by time, not by the order the curves were entered: an operator who records a later
/// campaign first and backfills the earlier one afterwards gets the same timeline either way.
#[tokio::test]
#[serial]
async fn an_out_of_order_insert_chains_by_time() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    let sensor = create_sensor(&db, "window-order", GLOBAL_PARAM_TEMP_ID).await;

    let (status, late) = create_curve(&app, &token, sensor.id, 3.0, "2025-03-01T00:00:00Z").await;
    assert_eq!(status, 201, "the later campaign is recorded first: {late}");
    let (status, early) = create_curve(&app, &token, sensor.id, 2.0, "2025-02-01T00:00:00Z").await;
    assert_eq!(
        status, 201,
        "the earlier campaign is backfilled afterwards: {early}"
    );

    let curves = curves_of(&db, sensor.id).await;
    assert_eq!(curves.len(), 3, "the bench base plus both campaigns");
    assert_eq!(
        curves[0].valid_until,
        Some(dt("2025-02-01T00:00:00Z")),
        "the bench base now ends where the earlier campaign opens"
    );
    assert_eq!(
        curves[1].valid_until,
        Some(dt("2025-03-01T00:00:00Z")),
        "the earlier campaign ends where the later one opens, whichever was entered first"
    );
    assert_eq!(
        curves[2].valid_until, None,
        "the last curve on the timeline stays open"
    );
}

/// Readings that predate a sensor's only curve stay uncorrected. Nothing invents a curve to cover
/// them, so reprocess clears the correction they were carrying instead of preserving it, and the
/// scientist's curve keeps the window it was entered with.
#[tokio::test]
#[serial]
async fn a_reading_before_the_first_curve_is_left_uncorrected() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    // No bench curve: the leading region has to be genuinely uncovered for the clear to be visible.
    let sensor_id = create_sensor_without_curve(&db, "window-leading").await;

    let (status, body) = post_json_parse_with_token(
        &app,
        "/api/sensor_calibrations",
        &json!({
            "sensor_id": sensor_id,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "slope": 2.0,
            "intercept": 0.0,
            "valid_from": "2025-02-01T00:00:00Z",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 201, "the scientist's curve is recorded: {body}");
    let real_curve: Uuid = e2e::id_of(&body).parse().expect("calibration uuid");

    let deployment = deploy_sensor(&db, sensor_id, SITE1_ID, dt("2024-01-01T00:00:00Z")).await;
    let stream = create_paired_stream(&db, "window-leading", PARAM_S1_TEMP_ID).await;
    for (time, raw) in [
        ("2025-01-15T00:00:00Z", 10.0),
        ("2025-03-15T00:00:00Z", 10.0),
    ] {
        exec_sql(
            &db,
            &format!(
                "INSERT INTO readings \
                 (stream_id, site_id, parameter_id, time, raw_value, calibrated_value, \
                  sensor_id, deployment_id, replicate_index) \
                 VALUES ('{stream}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{time}', {raw}, {raw}, \
                 '{sensor_id}', '{deployment}', 0)"
            ),
        )
        .await;
    }

    let (status, run) = post_json_parse_with_token(
        &app,
        "/api/actions/reprocess",
        &json!({ "sensor_id": sensor_id }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "reprocess the sensor ({status}): {run}");
    let job_id = run["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("reprocess returns a tracked job: {run}"))
        .to_string();
    assert_eq!(
        e2e::poll_job(&app, &token, &job_id, 60).await,
        "completed",
        "the reprocess job runs to completion"
    );

    let curves = curves_of(&db, sensor_id).await;
    assert_eq!(
        curves.len(),
        1,
        "the scientist's curve is the whole timeline: nothing is created to fill the gap ahead of it"
    );
    assert_eq!(
        curves[0].id, real_curve,
        "and it is the curve that was entered"
    );
    assert_eq!(
        curves[0].valid_from,
        dt("2025-02-01T00:00:00Z"),
        "opening where the operator said, not backdated over the leading readings"
    );
    assert_eq!(
        curves[0].valid_until, None,
        "and running to the end of time, unretracted"
    );

    let rows = get_readings(&db, stream).await;
    let leading = rows
        .iter()
        .find(|r| r.time == dt("2025-01-15T00:00:00Z"))
        .expect("the reading before the real curve");
    let covered = rows
        .iter()
        .find(|r| r.time == dt("2025-03-15T00:00:00Z"))
        .expect("the reading inside the real curve");
    assert_eq!(
        leading.calibration_id, None,
        "no curve covers the leading reading"
    );
    assert_eq!(
        leading.calibrated_value, None,
        "so the copy of its raw value it was carrying is cleared, not preserved"
    );
    assert_eq!(
        covered.calibration_id,
        Some(real_curve),
        "the later reading keeps the real curve"
    );
    assert_eq!(covered.calibrated_value, Some(20.0), "and carries 2*10");
}

/// A gap between two curves is as legal as the region before the first one, and reprocess treats it
/// the same way: a reading inside it names no curve and serves no corrected value, even though
/// curves exist on both sides of it.
#[tokio::test]
#[serial]
async fn a_reading_in_a_gap_between_two_curves_is_cleared() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    let sensor_id = create_sensor_without_curve(&db, "window-gap").await;
    let early = add_calibration_for_parameter(
        &db,
        sensor_id,
        GLOBAL_PARAM_TEMP_ID,
        2.0,
        0.0,
        dt("2025-01-01T00:00:00Z"),
    )
    .await;
    let late = add_calibration_for_parameter(
        &db,
        sensor_id,
        GLOBAL_PARAM_TEMP_ID,
        3.0,
        0.0,
        dt("2025-03-01T00:00:00Z"),
    )
    .await;
    // The operator retires the early curve a month before the next one starts, leaving February
    // uncovered on purpose.
    exec_sql(
        &db,
        &format!(
            "UPDATE sensor_calibrations \
             SET valid_until = '2025-02-01T00:00:00Z', valid_until_explicit = true \
             WHERE id = '{early}'"
        ),
    )
    .await;

    let deployment = deploy_sensor(&db, sensor_id, SITE1_ID, dt("2024-01-01T00:00:00Z")).await;
    let stream = create_paired_stream(&db, "window-gap", PARAM_S1_TEMP_ID).await;
    insert_readings(
        &db,
        stream,
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        sensor_id,
        early,
        deployment,
        2.0,
        0.0,
        &[
            (dt("2025-01-15T00:00:00Z"), 10.0),
            (dt("2025-02-15T00:00:00Z"), 10.0),
            (dt("2025-03-15T00:00:00Z"), 10.0),
        ],
    )
    .await;

    let (status, run) = post_json_parse_with_token(
        &app,
        "/api/actions/reprocess",
        &json!({ "sensor_id": sensor_id }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "reprocess the sensor ({status}): {run}");
    let job_id = run["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("reprocess returns a tracked job: {run}"))
        .to_string();
    assert_eq!(
        e2e::poll_job(&app, &token, &job_id, 60).await,
        "completed",
        "the reprocess job runs to completion"
    );

    let rows = get_readings(&db, stream).await;
    let at = |time: &str| {
        rows.iter()
            .find(|r| r.time == dt(time))
            .unwrap_or_else(|| panic!("no reading at {time}"))
    };

    let before = at("2025-01-15T00:00:00Z");
    assert_eq!(
        before.calibration_id,
        Some(early),
        "inside the early window"
    );
    assert_eq!(before.calibrated_value, Some(20.0), "carrying 2*10");

    let inside = at("2025-02-15T00:00:00Z");
    assert_eq!(
        inside.calibration_id, None,
        "the gap resolves no curve, and the stale reference to the retired one is dropped"
    );
    assert_eq!(
        inside.calibrated_value, None,
        "so the value that curve produced goes with it"
    );

    let after = at("2025-03-15T00:00:00Z");
    assert_eq!(after.calibration_id, Some(late), "inside the later window");
    assert_eq!(after.calibrated_value, Some(30.0), "carrying 3*10");
}

/// An instrument nobody has calibrated is an ordinary instrument: it deploys, it takes readings, and
/// a reprocess over it succeeds while leaving every value uncorrected.
#[tokio::test]
#[serial]
async fn a_sensor_with_no_curves_reprocesses_and_stays_uncorrected() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    let sensor_id = create_sensor_without_curve(&db, "window-uncalibrated").await;
    let deployment = deploy_sensor_for_parameter(
        &db,
        sensor_id,
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        dt("2024-01-01T00:00:00Z"),
    )
    .await;
    let stream = create_paired_stream(&db, "window-uncalibrated", PARAM_S1_TEMP_ID).await;
    insert_readings_without_curve(
        &db,
        stream,
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        sensor_id,
        deployment,
        &[
            (dt("2025-01-15T00:00:00Z"), 10.0),
            (dt("2025-02-15T00:00:00Z"), 20.0),
        ],
    )
    .await;

    let (status, run) = post_json_parse_with_token(
        &app,
        "/api/actions/reprocess",
        &json!({ "sensor_id": sensor_id }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "reprocess the sensor ({status}): {run}");
    let job_id = run["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("reprocess returns a tracked job: {run}"))
        .to_string();
    assert_eq!(
        e2e::poll_job(&app, &token, &job_id, 60).await,
        "completed",
        "a sensor with nothing to resolve is not an error"
    );

    assert!(
        curves_of(&db, sensor_id).await.is_empty(),
        "and no curve was created on its behalf"
    );

    let rows = get_readings(&db, stream).await;
    assert_eq!(rows.len(), 2, "both readings are still there: {rows:?}");
    for row in &rows {
        assert_eq!(
            row.sensor_id,
            Some(sensor_id),
            "the reading stays attributed to the instrument: {row:?}"
        );
        assert_eq!(
            row.deployment_id,
            Some(deployment),
            "and to its deployment: {row:?}"
        );
        assert_eq!(row.calibration_id, None, "with no curve to name: {row:?}");
        assert_eq!(
            row.calibrated_value, None,
            "and no corrected value: {row:?}"
        );
    }
}
