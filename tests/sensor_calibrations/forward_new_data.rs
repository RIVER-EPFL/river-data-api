//! A calibration recorded on top of the newest one: the new curve owns the data that arrives after
//! it, and the history the previous curve owns is left exactly as it was.
//!
//! Scenario: Track B is onboarded and its stream paired, a cycle of readings arrives, the operator
//! records C1 (slope 2.0, intercept 5.0) covering them, and later records C2 (slope 3.0, intercept
//! 1.0) valid from a date after every reading the site holds, then keeps ingesting.
//! Expected behaviour: C1's window auto-closes at C2's `valid_from`; readings before that boundary
//! keep C1's values and C1's `calibration_id`; readings at or after it resolve to C2; and the new
//! hourly bucket reports C2's mean while the old one still reports C1's.
//!
//! The coefficients are synthetic so every expected value is exact in binary floating point and
//! readable by eye (2*10+5 = 25, 3*10+1 = 31, (31+61)/2 = 46). No tolerance is used anywhere.
//!
//! Fixture times sit in June and July 2025. They are in the past because the aggregate refresh
//! window is `[since, NOW()]`, and they are kept clear of the January and mid-June dates the other
//! suites materialise buckets on.

use axum::Router;
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::common::e2e::{self, hourly_bucket, poll_job, refresh_hourly};
use crate::common::keycloak as kc;
use crate::common::sensor_lifecycle::{ReadingRow, dt, get_readings};
use crate::common::tracks;

const JOB_WAIT: Duration = Duration::from_secs(30);

/// C1 opens on the day Track B's deployment opens.
const T0: &str = "2025-06-02T00:00:00Z";
/// C2 opens after every reading the fixture holds.
const T2: &str = "2025-07-01T00:00:00Z";

const H1: &str = "2025-06-10T10:00:00Z";
const H2: &str = "2025-06-10T10:20:00Z";
const H3: &str = "2025-06-10T10:40:00Z";
/// The hourly bucket the three history readings fall in.
const HISTORY_BUCKET: &str = "2025-06-10T10:00:00Z";

/// Inside C1's window, ingested after C1 exists.
const LATE_C1: &str = "2025-06-25T09:00:00Z";
/// Inside C1's window, back-dated relative to C2.
const BACKDATED: &str = "2025-06-20T10:00:00Z";

const F1: &str = "2025-07-05T10:00:00Z";
const F2: &str = "2025-07-05T10:20:00Z";
/// The hourly bucket the two forward readings fall in.
const FORWARD_BUCKET: &str = "2025-07-05T10:00:00Z";

// ============================================================================
// Helpers
// ============================================================================

async fn ingest(
    app: &Router,
    token: &str,
    stream_id: &str,
    rows: &[(&str, f64)],
) -> (u16, String) {
    let readings: Vec<serde_json::Value> = rows
        .iter()
        .map(|(time, raw)| json!({ "time": time, "raw_value": raw }))
        .collect();
    crate::common::post_json_with_token(
        app,
        "/api/ingest",
        &json!({ "stream_id": stream_id, "readings": readings }),
        token,
    )
    .await
}

/// One discrete ingest cycle, the Track B update mechanism: one HTTP call carrying the batch.
async fn ingest_cycle(app: &Router, token: &str, stream_id: &str, rows: &[(&str, f64)]) {
    let (status, body) = ingest(app, token, stream_id, rows).await;
    assert_eq!(status, 200, "ingest cycle of {} readings: {body}", rows.len());
    let parsed: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("ingest response is JSON ({e}): {body}"));
    assert_eq!(
        parsed["inserted"].as_u64(),
        Some(rows.len() as u64),
        "every reading in the cycle is stored: {parsed}"
    );
}

/// Wait for the tracked job a specific entity triggered. Keyed on `trigger_id` (the calibration's
/// own id), not merely on `trigger_type`: an earlier calibration's job already satisfies the
/// type-wide helper, so it would report success even if this create enqueued nothing.
async fn wait_for_job(
    db: &DatabaseConnection,
    trigger_type: &str,
    trigger_id: &str,
    timeout: Duration,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT status FROM reprocessing_jobs \
                 WHERE trigger_type = $1 AND trigger_id = $2::uuid",
                [trigger_type.into(), trigger_id.into()],
            ))
            .await
            .expect("query reprocessing_jobs");
        let status: Option<String> = row.and_then(|r| r.try_get::<String>("", "status").ok());
        let settled = matches!(status.as_deref(), Some("completed" | "failed" | "cancelled"));
        if settled || Instant::now() >= deadline {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A sensor's calibrations, oldest window first.
async fn calibrations_for(app: &Router, token: &str, sensor_id: &str) -> Vec<serde_json::Value> {
    let filter = e2e::percent_encode(&format!(r#"{{"sensor_id":"{sensor_id}"}}"#));
    let (status, body) = crate::common::get_json_with_token(
        app,
        &format!("/api/sensor_calibrations?filter={filter}"),
        token,
    )
    .await;
    assert_eq!(status, 200, "list calibrations for {sensor_id}: {body}");
    let mut cals = body
        .as_array()
        .unwrap_or_else(|| panic!("the calibration list is an array: {body}"))
        .clone();
    cals.sort_by_key(valid_from);
    cals
}

fn valid_from(cal: &serde_json::Value) -> DateTime<Utc> {
    dt(cal["valid_from"]
        .as_str()
        .unwrap_or_else(|| panic!("a calibration carries valid_from: {cal}")))
}

fn valid_until(cal: &serde_json::Value) -> Option<DateTime<Utc>> {
    cal["valid_until"].as_str().map(dt)
}

async fn window(app: &Router, token: &str, calibration_id: &str) -> serde_json::Value {
    let (status, body) = crate::common::get_json_with_token(
        app,
        &format!("/api/sensor_calibrations/{calibration_id}/window"),
        token,
    )
    .await;
    assert_eq!(status, 200, "calibration window for {calibration_id}: {body}");
    body
}

/// The calibrated values a window resolves, chronologically.
fn window_values(w: &serde_json::Value) -> Vec<f64> {
    let points = w["points"]
        .as_array()
        .unwrap_or_else(|| panic!("a window response carries points: {w}"));
    points
        .iter()
        .map(|p| {
            p["calibrated_value"]
                .as_f64()
                .unwrap_or_else(|| panic!("a window point carries calibrated_value: {p}"))
        })
        .collect()
}

fn reading_at<'a>(rows: &'a [ReadingRow], time: &str) -> &'a ReadingRow {
    let want = dt(time);
    rows.iter().find(|r| r.time == want).unwrap_or_else(|| {
        let held: Vec<String> = rows.iter().map(|r| r.time.to_rfc3339()).collect();
        panic!("no reading at {time}; the stream holds {held:?}")
    })
}

fn bucket_index(resp: &serde_json::Value, bucket: &str) -> usize {
    let want = dt(bucket);
    resp["times"]
        .as_array()
        .unwrap_or_else(|| panic!("the aggregates response carries times: {resp}"))
        .iter()
        .position(|t| t.as_str().is_some_and(|s| dt(s) == want))
        .unwrap_or_else(|| panic!("no {bucket} bucket in the aggregates response: {resp}"))
}

async fn create_calibration(
    app: &Router,
    token: &str,
    sensor_id: &str,
    slope: f64,
    intercept: f64,
    valid_from: &str,
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        app,
        "/api/sensor_calibrations",
        &json!({
            "sensor_id": sensor_id,
            "slope": slope,
            "intercept": intercept,
            "valid_from": valid_from,
        }),
        token,
    )
    .await
}

struct Fixture {
    track: tracks::Track,
    stream_id: String,
    stream_uuid: Uuid,
    sensor_id: String,
    sensor_uuid: Uuid,
    identity_id: String,
    c1_id: String,
    c1_uuid: Uuid,
}

impl Fixture {
    fn site_uuid(&self) -> Uuid {
        Uuid::parse_str(&self.track.site_id).expect("site id is a uuid")
    }

    fn parameter_id(&self) -> &str {
        self.track.parameter_id("TrkFlowDO")
    }
}

/// Track B onboarded and paired, three history readings ingested, and C1 (2.0 / 5.0) recorded over
/// them. Returns with C1's tracked reprocess already applied, so the base state is proven rather
/// than assumed.
async fn arrange(db: &DatabaseConnection, app: &Router, admin: &str) -> Fixture {
    let track = tracks::onboard_sensor_flow_track(app, admin).await;
    let stream_id = track.stream_ids[0].clone();
    let stream_uuid = Uuid::parse_str(&stream_id).expect("stream id is a uuid");

    // Without the link the readings would be owned by the track's deployed sensor (reprocess
    // re-derives ownership from the deployment window) while the calibrations sat on the minted
    // one, so the link is what keeps one timeline for one feed.
    let sensor_hint = track
        .sensor_id
        .clone()
        .expect("the sensor-flow track provisions a sensor");
    e2e::link_stream_sensor(app, admin, &stream_id, &sensor_hint).await;

    // Paired before the first cycle, the steady state a sync feed runs in: every reading below is
    // attributed at write time rather than by a backfill.
    let (status, paired) = crate::common::post_json_parse_with_token(
        app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({ "site_parameter_id": track.site_parameter_ids[0] }),
        admin,
    )
    .await;
    assert!((200..300).contains(&status), "pair the stream ({status}): {paired}");
    assert_eq!(
        paired["backfilled"].as_u64(),
        Some(0),
        "the stream carries no readings yet, so pairing has nothing to backfill: {paired}"
    );

    // Pairing binds the stream to a sensor and gives that sensor an identity curve. That sensor's
    // calibration timeline is the one these readings resolve against, so it is the one the operator
    // records calibrations on.
    let sensor_id = paired["stream"]["sensor_id"]
        .as_str()
        .unwrap_or_else(|| panic!("pairing binds the stream to a sensor: {paired}"))
        .to_string();
    let sensor_uuid = Uuid::parse_str(&sensor_id).expect("sensor id is a uuid");

    let cals = calibrations_for(app, admin, &sensor_id).await;
    assert_eq!(
        cals.len(),
        1,
        "pairing leaves exactly one (identity) curve on the stream's sensor: {cals:?}"
    );
    let identity_id = cals[0]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the identity curve carries an id: {cals:?}"))
        .to_string();

    ingest_cycle(app, admin, &stream_id, &[(H1, 10.0), (H2, 20.0), (H3, 30.0)]).await;

    let (status, c1) = create_calibration(app, admin, &sensor_id, 2.0, 5.0, T0).await;
    assert_eq!(status, 201, "record C1 over the deployment: {c1}");
    let c1_id = e2e::id_of(&c1);
    let c1_uuid = Uuid::parse_str(&c1_id).expect("calibration id is a uuid");

    assert_eq!(
        wait_for_job(db, "calibration_create", &c1_id, JOB_WAIT).await.as_deref(),
        Some("completed"),
        "recording a calibration enqueues a tracked reprocess for that calibration"
    );

    let rows = get_readings(db, stream_uuid).await;
    assert_eq!(rows.len(), 3, "the history cycle is the whole stream so far: {rows:?}");
    for (time, raw, calibrated) in [(H1, 10.0, 25.0), (H2, 20.0, 45.0), (H3, 30.0, 65.0)] {
        let row = reading_at(&rows, time);
        assert_eq!(row.raw_value, raw, "{time} keeps its raw value");
        assert_eq!(
            row.calibrated_value,
            Some(calibrated),
            "{time}: C1 gives 2*{raw}+5 = {calibrated}"
        );
        assert_eq!(
            row.calibration_id,
            Some(c1_uuid),
            "{time} is stamped with C1, not the identity curve pairing left"
        );
        assert_eq!(row.site_id, Some(Uuid::parse_str(&track.site_id).unwrap()), "{time} stays attributed to the site");
    }

    Fixture {
        track,
        stream_id,
        stream_uuid,
        sensor_id,
        sensor_uuid,
        identity_id,
        c1_id,
        c1_uuid,
    }
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
#[serial]
async fn forward_calibration_closes_the_previous_window_and_leaves_history_alone() {
    if !kc::require_keycloak_or_skip("forward_calibration_closes_the_previous_window").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let fx = arrange(&db, &app, &admin).await;

    let before = calibrations_for(&app, &admin, &fx.sensor_id).await;
    assert_eq!(before.len(), 2, "C1 plus the identity curve: {before:?}");
    assert_eq!(before[0]["id"], fx.c1_id.as_str(), "C1 opens the timeline: {before:?}");
    let identity_from = valid_from(&before[1]);
    assert_eq!(
        valid_until(&before[0]),
        Some(identity_from),
        "C1's window runs up to the next curve on the timeline: {before:?}"
    );

    // A reading that arrives while C1 is still the newest field curve. Ingest resolves the window
    // covering the reading time and stores the corrected value, so this row carries 2*40+5 from the
    // moment it lands, and a later reprocess recomputes the same number.
    ingest_cycle(&app, &admin, &fx.stream_id, &[(LATE_C1, 40.0)]).await;
    let rows = get_readings(&db, fx.stream_uuid).await;
    let probe = reading_at(&rows, LATE_C1);
    assert_eq!(
        probe.calibrated_value,
        Some(85.0),
        "POST /ingest applies the curve covering the reading time (2*40+5)"
    );
    assert_eq!(
        probe.calibration_id,
        Some(fx.c1_uuid),
        "the write path resolves the window covering the reading time"
    );

    let (status, c2) = create_calibration(&app, &admin, &fx.sensor_id, 3.0, 1.0, T2).await;
    assert_eq!(status, 201, "record C2 on top of C1: {c2}");
    let c2_id = e2e::id_of(&c2);
    assert_eq!(
        wait_for_job(&db, "calibration_create", &c2_id, JOB_WAIT).await.as_deref(),
        Some("completed"),
        "recording C2 enqueues a tracked reprocess for C2"
    );

    let c1_window = window(&app, &admin, &fx.c1_id).await;
    assert_eq!(
        c1_window["valid_until"].as_str().map(dt),
        Some(dt(T2)),
        "adding C2 closes C1's window at C2's valid_from: {c1_window}"
    );
    assert_eq!(c1_window["slope"], 2.0, "C1's coefficients are untouched: {c1_window}");
    assert_eq!(c1_window["intercept"], 5.0, "C1's coefficients are untouched: {c1_window}");
    assert_eq!(
        c1_window["point_count"].as_i64(),
        Some(4),
        "C1 still owns all four pre-T2 readings: {c1_window}"
    );
    assert_eq!(
        window_values(&c1_window),
        vec![25.0, 45.0, 65.0, 85.0],
        "the whole-sensor reprocess re-derived C1's window with C1's curve: {c1_window}"
    );

    let c2_window = window(&app, &admin, &c2_id).await;
    assert_eq!(
        c2_window["valid_from"].as_str().map(dt),
        Some(dt(T2)),
        "C2 opens where it was recorded: {c2_window}"
    );
    assert_eq!(
        c2_window["valid_until"].as_str().map(dt),
        Some(identity_from),
        "C2 takes over C1's former upper bound, the next curve on the timeline: {c2_window}"
    );
    assert_eq!(
        c2_window["point_count"].as_i64(),
        Some(0),
        "the forward curve owns no data yet: {c2_window}"
    );
    assert!(
        window_values(&c2_window).is_empty(),
        "the forward curve owns no data yet: {c2_window}"
    );

    let chain = calibrations_for(&app, &admin, &fx.sensor_id).await;
    assert_eq!(chain.len(), 3, "identity, C1 and C2: {chain:?}");
    for pair in chain.windows(2) {
        assert_eq!(
            valid_until(&pair[0]),
            Some(valid_from(&pair[1])),
            "each window is closed at the next one's start (calibration coverage is continuous): {chain:?}"
        );
    }
    assert!(
        valid_until(&chain[2]).is_none(),
        "the last window on the timeline stays open: {chain:?}"
    );
    assert_eq!(
        chain[2]["id"], fx.identity_id.as_str(),
        "the identity curve pairing created is the last link: {chain:?}"
    );

    let rows = get_readings(&db, fx.stream_uuid).await;
    assert_eq!(rows.len(), 4, "no reading was added or lost: {rows:?}");
    for (time, raw, calibrated) in [
        (H1, 10.0, 25.0),
        (H2, 20.0, 45.0),
        (H3, 30.0, 65.0),
        (LATE_C1, 40.0, 85.0),
    ] {
        let row = reading_at(&rows, time);
        assert_eq!(row.raw_value, raw, "{time} keeps its raw value");
        assert_eq!(
            row.calibrated_value,
            Some(calibrated),
            "{time} stays on C1's curve: 2*{raw}+5 = {calibrated}"
        );
        assert_eq!(
            row.calibration_id,
            Some(fx.c1_uuid),
            "{time} keeps C1 as its calibration, C2 owns nothing before T2"
        );
    }

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn readings_ingested_after_the_new_calibration_carry_it_and_take_its_curve_on_reprocess() {
    if !kc::require_keycloak_or_skip("readings_ingested_after_the_new_calibration").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let fx = arrange(&db, &app, &admin).await;

    let (status, c2) = create_calibration(&app, &admin, &fx.sensor_id, 3.0, 1.0, T2).await;
    assert_eq!(status, 201, "record C2 on top of C1: {c2}");
    let c2_id = e2e::id_of(&c2);
    let c2_uuid = Uuid::parse_str(&c2_id).expect("calibration id is a uuid");
    assert_eq!(
        wait_for_job(&db, "calibration_create", &c2_id, JOB_WAIT).await.as_deref(),
        Some("completed"),
        "recording C2 enqueues a tracked reprocess for C2"
    );

    kc::ensure_realm_user("intern1", "intern1", &["riverdata-intern"]).await;
    kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
    kc::ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    for user in ["intern1", "river1", "manager1"] {
        let sub = kc::keycloak_user_id(user).await;
        kc::grant_project(&db, &sub, &fx.track.project_id).await;
    }
    let intern = kc::get_keycloak_jwt("intern1", "intern1").await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;
    let manager = kc::get_keycloak_jwt("manager1", "manager1").await;

    let forward = [(F1, 10.0), (F2, 20.0)];
    let (status, denied) = ingest(&app, &intern, &fx.stream_id, &forward).await;
    assert_eq!(
        status, 403,
        "pushing data is a RIVER-level write, an intern on the same project may not: {denied}"
    );

    ingest_cycle(&app, &river, &fx.stream_id, &forward).await;

    let rows = get_readings(&db, fx.stream_uuid).await;
    assert_eq!(rows.len(), 5, "three history readings plus the new cycle: {rows:?}");
    for (time, raw) in forward {
        let row = reading_at(&rows, time);
        assert_eq!(
            row.calibration_id,
            Some(c2_uuid),
            "{time} resolves the forward window at write time, not the latest-created curve by rank"
        );
        assert_eq!(
            row.calibrated_value,
            Some(3.0 * raw + 1.0),
            "POST /ingest applies C2, the curve covering {time}"
        );
        assert_eq!(row.sensor_id, Some(fx.sensor_uuid), "{time} is owned by the stream's sensor");
        assert_eq!(row.site_id, Some(fx.site_uuid()), "{time} lands attributed to the site");
    }

    let (status, refused) = crate::common::post_json_with_token(
        &app,
        "/api/actions/reprocess",
        &json!({ "sensor_id": fx.sensor_id }),
        &river,
    )
    .await;
    assert_eq!(
        status, 403,
        "reprocessing a sensor is a MANAGER action, a RIVER member may not: {refused}"
    );

    let (status, job) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/reprocess",
        &json!({ "sensor_id": fx.sensor_id }),
        &manager,
    )
    .await;
    assert_eq!(status, 200, "a manager reprocesses the sensor: {job}");
    let job_id = job["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("reprocess returns a tracked job id: {job}"))
        .to_string();
    assert_eq!(
        poll_job(&app, &admin, &job_id, 30).await,
        "completed",
        "the manual reprocess job runs to completion"
    );

    let rows = get_readings(&db, fx.stream_uuid).await;
    for (time, raw, calibrated) in [(F1, 10.0, 31.0), (F2, 20.0, 61.0)] {
        let row = reading_at(&rows, time);
        assert_eq!(row.raw_value, raw, "{time} keeps its raw value");
        assert_eq!(
            row.calibrated_value,
            Some(calibrated),
            "{time} takes C2's curve: 3*{raw}+1 = {calibrated}"
        );
        assert_eq!(row.calibration_id, Some(c2_uuid), "{time} stays stamped with C2");
    }
    for (time, raw, calibrated) in [(H1, 10.0, 25.0), (H2, 20.0, 45.0), (H3, 30.0, 65.0)] {
        let row = reading_at(&rows, time);
        assert_eq!(row.raw_value, raw, "{time} keeps its raw value");
        assert_eq!(
            row.calibrated_value,
            Some(calibrated),
            "{time} stays on C1's curve: 2*{raw}+5 = {calibrated}"
        );
        assert_eq!(
            row.calibration_id,
            Some(fx.c1_uuid),
            "{time} keeps C1, a whole-sensor reprocess must not drag history onto the new curve"
        );
    }

    let c1_window = window(&app, &admin, &fx.c1_id).await;
    assert_eq!(
        c1_window["point_count"].as_i64(),
        Some(3),
        "C1's window still resolves exactly the history: {c1_window}"
    );
    assert_eq!(
        window_values(&c1_window),
        vec![25.0, 45.0, 65.0],
        "C1's window reports C1's values: {c1_window}"
    );

    let c2_window = window(&app, &admin, &c2_id).await;
    assert_eq!(
        c2_window["point_count"].as_i64(),
        Some(2),
        "C2's window resolves the new cycle: {c2_window}"
    );
    assert_eq!(
        window_values(&c2_window),
        vec![31.0, 61.0],
        "C2's window reports C2's values: {c2_window}"
    );

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn a_backdated_reading_resolves_to_the_older_window_not_the_latest_calibration() {
    if !kc::require_keycloak_or_skip("backdated_reading_resolves_to_the_older_window").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    kc::ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    let manager = kc::get_keycloak_jwt("manager1", "manager1").await;

    let fx = arrange(&db, &app, &admin).await;
    kc::grant_project(&db, &kc::keycloak_user_id("manager1").await, &fx.track.project_id).await;

    let (status, c2) = create_calibration(&app, &admin, &fx.sensor_id, 3.0, 1.0, T2).await;
    assert_eq!(status, 201, "record C2 on top of C1: {c2}");
    let c2_id = e2e::id_of(&c2);
    let c2_uuid = Uuid::parse_str(&c2_id).expect("calibration id is a uuid");
    assert_eq!(
        wait_for_job(&db, "calibration_create", &c2_id, JOB_WAIT).await.as_deref(),
        Some("completed"),
        "recording C2 enqueues a tracked reprocess for C2"
    );

    // One cycle carrying a back-dated row, a row exactly on C2's boundary, and a forward row, so
    // the split is decided inside a single write.
    ingest_cycle(
        &app,
        &admin,
        &fx.stream_id,
        &[(BACKDATED, 40.0), (T2, 2.0), (F1, 10.0)],
    )
    .await;

    let rows = get_readings(&db, fx.stream_uuid).await;
    assert_eq!(
        reading_at(&rows, BACKDATED).calibration_id,
        Some(fx.c1_uuid),
        "a time inside the closed window resolves to C1, not to the newest calibration"
    );
    assert_eq!(
        reading_at(&rows, T2).calibration_id,
        Some(c2_uuid),
        "valid_from is inclusive: a reading exactly at the boundary belongs to C2"
    );
    assert_eq!(
        reading_at(&rows, F1).calibration_id,
        Some(c2_uuid),
        "a time after the boundary belongs to C2"
    );

    let (status, job) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/reprocess",
        &json!({ "sensor_id": fx.sensor_id }),
        &manager,
    )
    .await;
    assert_eq!(status, 200, "a manager reprocesses the sensor: {job}");
    let job_id = job["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("reprocess returns a tracked job id: {job}"))
        .to_string();
    assert_eq!(
        poll_job(&app, &admin, &job_id, 30).await,
        "completed",
        "the manual reprocess job runs to completion"
    );

    let rows = get_readings(&db, fx.stream_uuid).await;
    assert_eq!(rows.len(), 6, "three history readings plus the three-row cycle: {rows:?}");
    for (time, raw, calibrated, cal) in [
        (BACKDATED, 40.0, 85.0, fx.c1_uuid),
        (T2, 2.0, 7.0, c2_uuid),
        (F1, 10.0, 31.0, c2_uuid),
    ] {
        let row = reading_at(&rows, time);
        assert_eq!(row.raw_value, raw, "{time} keeps its raw value");
        assert_eq!(
            row.calibrated_value,
            Some(calibrated),
            "{time} is calibrated by the window covering it, giving {calibrated}"
        );
        assert_eq!(row.calibration_id, Some(cal), "{time} keeps the window it resolved at write time");
    }
    for (time, raw, calibrated) in [(H1, 10.0, 25.0), (H2, 20.0, 45.0), (H3, 30.0, 65.0)] {
        let row = reading_at(&rows, time);
        assert_eq!(row.raw_value, raw, "{time} keeps its raw value");
        assert_eq!(row.calibrated_value, Some(calibrated), "{time} stays on C1's curve");
        assert_eq!(row.calibration_id, Some(fx.c1_uuid), "{time} keeps C1");
    }

    let c1_window = window(&app, &admin, &fx.c1_id).await;
    assert_eq!(
        c1_window["point_count"].as_i64(),
        Some(4),
        "C1's window takes in the back-dated reading: {c1_window}"
    );
    assert_eq!(
        window_values(&c1_window),
        vec![25.0, 45.0, 65.0, 85.0],
        "no C2-derived value leaked into C1's window: {c1_window}"
    );

    let c2_window = window(&app, &admin, &c2_id).await;
    assert_eq!(
        c2_window["point_count"].as_i64(),
        Some(2),
        "C2's window holds the boundary reading and the forward one: {c2_window}"
    );
    assert_eq!(
        window_values(&c2_window),
        vec![7.0, 31.0],
        "C2's window reports C2's values: {c2_window}"
    );

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn aggregates_report_the_new_curve_for_the_new_bucket_and_leave_the_old_bucket_alone() {
    if !kc::require_keycloak_or_skip("aggregates_report_the_new_curve").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;
    kc::ensure_realm_user("manager1", "manager1", &["riverdata-manager"]).await;
    let manager = kc::get_keycloak_jwt("manager1", "manager1").await;

    let fx = arrange(&db, &app, &admin).await;
    kc::grant_project(&db, &kc::keycloak_user_id("manager1").await, &fx.track.project_id).await;
    let parameter_id = fx.parameter_id().to_string();

    refresh_hourly(&db, dt("2025-06-01T00:00:00Z")).await;
    let history = hourly_bucket(&db, &fx.track.site_id, &parameter_id, dt(HISTORY_BUCKET)).await;
    assert!(
        history.is_some(),
        "the history bucket materialises once the aggregate is refreshed over its range"
    );
    let (mean, count) = history.unwrap();
    assert_eq!(mean, 45.0, "C1's bucket mean is (25+45+65)/3");
    assert_eq!(count, 3, "three readings roll up");

    let (status, c2) = create_calibration(&app, &admin, &fx.sensor_id, 3.0, 1.0, T2).await;
    assert_eq!(status, 201, "record C2 on top of C1: {c2}");
    let c2_id = e2e::id_of(&c2);
    assert_eq!(
        wait_for_job(&db, "calibration_create", &c2_id, JOB_WAIT).await.as_deref(),
        Some("completed"),
        "recording C2 enqueues a tracked reprocess for C2"
    );

    ingest_cycle(&app, &admin, &fx.stream_id, &[(F1, 10.0), (F2, 20.0)]).await;

    let (status, job) = crate::common::post_json_parse_with_token(
        &app,
        "/api/actions/reprocess",
        &json!({ "sensor_id": fx.sensor_id }),
        &manager,
    )
    .await;
    assert_eq!(status, 200, "a manager reprocesses the sensor: {job}");
    let job_id = job["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("reprocess returns a tracked job id: {job}"))
        .to_string();
    assert_eq!(
        poll_job(&app, &admin, &job_id, 30).await,
        "completed",
        "the manual reprocess job runs to completion"
    );

    // A completed job is not evidence of a refresh (refresh errors are swallowed by a warn), so the
    // bucket values are what the assertions read.
    let forward = hourly_bucket(&db, &fx.track.site_id, &parameter_id, dt(FORWARD_BUCKET)).await;
    assert!(
        forward.is_some(),
        "the reprocess refreshes the aggregate from the sensor's earliest reading, so the new bucket exists"
    );
    let (mean, count) = forward.unwrap();
    assert_eq!(mean, 46.0, "C2's bucket mean is (31+61)/2; the uncalibrated mean would be 15.0");
    assert_eq!(count, 2, "two readings roll up");

    let history = hourly_bucket(&db, &fx.track.site_id, &parameter_id, dt(HISTORY_BUCKET)).await;
    assert!(history.is_some(), "the history bucket survives the refresh");
    let (mean, count) = history.unwrap();
    assert_eq!(mean, 45.0, "the older bucket still reports C1's mean");
    assert_eq!(count, 3, "and still holds three readings");

    let (status, served) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/sites/{}/aggregates/hourly?start=2025-06-01T00:00:00Z&end=2025-07-31T00:00:00Z",
            fx.track.site_id
        ),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "the site's hourly aggregates are served: {served}");
    let forward_idx = bucket_index(&served, FORWARD_BUCKET);
    let history_idx = bucket_index(&served, HISTORY_BUCKET);
    let avg = e2e::field_for(&served, &parameter_id, "avg");
    let counts = e2e::field_for(&served, &parameter_id, "count");
    assert_eq!(avg[forward_idx], 46.0, "the served new bucket carries C2's mean: {served}");
    assert_eq!(counts[forward_idx], 2.0, "two readings in the served new bucket: {served}");
    assert_eq!(avg[history_idx], 45.0, "the served old bucket still carries C1's mean: {served}");
    assert_eq!(counts[history_idx], 3.0, "three readings in the served old bucket: {served}");

    crate::common::cleanup_test_db(&db).await;
}
