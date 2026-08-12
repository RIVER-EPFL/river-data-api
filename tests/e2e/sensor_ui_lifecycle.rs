//! The sensor detail page's lifecycle actions, driven the way the dashboard drives them.
//!
//! Scenario: an operator recalls, re-opens, backdates and redeploys a sensor from the sensor page,
//! and backfills calibration coverage from the sensors list.
//! Expected behaviour: attribution is always re-derivable from the deployment and calibration
//! timelines, so every edit re-attributes the readings its window covers and leaves the rest alone.
//!
//! The dashboard sends `api.sensorDeployments.update` (`PUT /api/sensor_deployments/{id}`,
//! `src/lib/api/crud.ts`) for recall, date edits and the backdate button, and
//! `api.sensorDeployments.create` for a (re)deploy; it reads `slot_data_start` from
//! `GET /sensors/{id}/readings` to pick the backdate target, and drives
//! `/actions/calibration_candidates` + `/actions/backfill_calibrations` from the sensors list.
//! Those are the surfaces exercised here, not the operator POST endpoints other suites cover.
//!
//! Every fixture is provisioned over HTTP from an empty database as Track B: project, site,
//! parameter, sensor, deployment, registered stream, pairing, then ingest cycles. SQL appears only
//! to read columns no endpoint exposes (`readings.deployment_id`, `sensor_calibrations.valid_from`).
//! Each step runs as the lowest role that should be able to perform it, and the role below it is
//! asserted to be refused.

use axum::Router;
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::common::e2e::count;
use crate::common::keycloak as kc;
use crate::common::{e2e, tracks};

// ---------------------------------------------------------------------------
// Fixture time
// ---------------------------------------------------------------------------

/// An instant on the sensor-flow track's fixture day, `secs` after midnight. The day is in the
/// past: the aggregate refresh window is `[since, NOW()]`, so a future-dated fixture is never
/// materialised.
fn flow_at(secs: i64) -> String {
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        tracks::FLOW_BASE_DAY,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn flow_dt(secs: i64) -> DateTime<Utc> {
    dt(&flow_at(secs))
}

fn dt(s: &str) -> DateTime<Utc> {
    s.parse().unwrap_or_else(|e| panic!("invalid fixture instant '{s}': {e}"))
}

fn uid(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap_or_else(|e| panic!("invalid uuid '{s}': {e}"))
}

/// A timestamp field of a JSON response, parsed rather than string-compared.
fn ts(body: &serde_json::Value, key: &str) -> Option<DateTime<Utc>> {
    body[key].as_str().and_then(|s| s.parse::<DateTime<Utc>>().ok())
}

/// Historical CSV rows for one parameter column, the shape the importer accepts.
fn history_csv(code: &str) -> String {
    format!("DateTime,{code}\n2025-05-01T00:00:00Z,250.00\n2025-05-15T00:00:00Z,260.00\n")
}

const HISTORY_START: &str = "2025-05-01T00:00:00Z";

// ---------------------------------------------------------------------------
// DB readback (verification only, never state creation)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct SlotReading {
    time: DateTime<Utc>,
    raw_value: f64,
    calibrated_value: Option<f64>,
    site_id: Option<Uuid>,
    sensor_id: Option<Uuid>,
    calibration_id: Option<Uuid>,
    deployment_id: Option<Uuid>,
}

/// Every reading at a parameter, ordered by time. Keyed on parameter rather than site so a recall's
/// `site_id` NULL-clear cannot hide rows from the assertions that check for it.
async fn slot_readings(db: &DatabaseConnection, parameter_id: &str) -> Vec<SlotReading> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT time, raw_value, calibrated_value, site_id, sensor_id, calibration_id, \
                    deployment_id \
             FROM readings WHERE parameter_id = $1 ORDER BY time",
            [uid(parameter_id).into()],
        ))
        .await
        .expect("query readings");

    rows.iter()
        .map(|r| {
            let time: DateTime<chrono::FixedOffset> = r.try_get("", "time").expect("time");
            SlotReading {
                time: time.with_timezone(&Utc),
                raw_value: r.try_get("", "raw_value").expect("raw_value"),
                calibrated_value: r.try_get("", "calibrated_value").ok(),
                site_id: r.try_get("", "site_id").ok(),
                sensor_id: r.try_get("", "sensor_id").ok(),
                calibration_id: r.try_get("", "calibration_id").ok(),
                deployment_id: r.try_get("", "deployment_id").ok(),
            }
        })
        .collect()
}

async fn deployment_window(
    db: &DatabaseConnection,
    deployment_id: &str,
) -> (DateTime<Utc>, Option<DateTime<Utc>>) {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT deployed_from, deployed_until FROM sensor_deployments WHERE id = $1",
            [uid(deployment_id).into()],
        ))
        .await
        .expect("query sensor_deployments")
        .expect("deployment row");
    let from: DateTime<chrono::FixedOffset> = row.try_get("", "deployed_from").expect("deployed_from");
    let until = row
        .try_get::<DateTime<chrono::FixedOffset>>("", "deployed_until")
        .ok()
        .map(|t| t.with_timezone(&Utc));
    (from.with_timezone(&Utc), until)
}

struct CurveRow {
    id: Uuid,
    slope: f64,
    intercept: f64,
    valid_from: DateTime<Utc>,
    valid_until: Option<DateTime<Utc>>,
}

/// The sensor's earliest calibration window.
async fn earliest_curve(db: &DatabaseConnection, sensor_id: &str) -> CurveRow {
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, slope, intercept, valid_from, valid_until FROM sensor_calibrations \
             WHERE sensor_id = $1 ORDER BY valid_from LIMIT 1",
            [uid(sensor_id).into()],
        ))
        .await
        .expect("query sensor_calibrations")
        .expect("calibration row");
    let valid_from: DateTime<chrono::FixedOffset> = row.try_get("", "valid_from").expect("valid_from");
    CurveRow {
        id: row.try_get("", "id").expect("id"),
        slope: row.try_get("", "slope").expect("slope"),
        intercept: row.try_get("", "intercept").expect("intercept"),
        valid_from: valid_from.with_timezone(&Utc),
        valid_until: row
            .try_get::<DateTime<chrono::FixedOffset>>("", "valid_until")
            .ok()
            .map(|t| t.with_timezone(&Utc)),
    }
}

/// Wait until at least `expected` jobs of `trigger_type` are terminal, returning
/// `(completed, failed)`. An enqueue that never happened times out and returns a count below
/// `expected`, so the caller's equality assertion fails instead of passing silently.
async fn settled_jobs(
    db: &DatabaseConnection,
    trigger_type: &str,
    expected: i64,
    timeout_secs: u64,
) -> (i64, i64) {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT \
                   COUNT(*) FILTER (WHERE status = 'completed') AS completed, \
                   COUNT(*) FILTER (WHERE status = 'failed') AS failed, \
                   COUNT(*) FILTER (WHERE status IN ('queued','pending','running','retrying')) AS active \
                 FROM reprocessing_jobs WHERE trigger_type = $1",
                [trigger_type.into()],
            ))
            .await
            .expect("query reprocessing_jobs")
            .expect("count row");
        let completed: i64 = row.try_get("", "completed").expect("completed");
        let failed: i64 = row.try_get("", "failed").expect("failed");
        let active: i64 = row.try_get("", "active").expect("active");
        if active == 0 && completed + failed >= expected {
            return (completed, failed);
        }
        if Instant::now() >= deadline {
            return (completed, failed);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---------------------------------------------------------------------------
// Actors and fixture
// ---------------------------------------------------------------------------

/// A real Keycloak user at one access level, granted the track's project. Capability alone is not
/// access here: a non-admin member is confined to the projects they hold a grant for.
async fn actor(db: &DatabaseConnection, username: &str, role: &str, project_id: &str) -> String {
    kc::ensure_realm_user(username, username, &[role]).await;
    kc::grant_project(db, &kc::keycloak_user_id(username).await, project_id).await;
    kc::get_keycloak_jwt(username, username).await
}

struct Flow {
    track: tracks::Track,
    sensor: String,
    deployment: String,
    stream: String,
    parameter: String,
    parameter_code: String,
}

/// Track B onboarded and paired: the state a slot is in once a sync service feeds it.
///
/// Pairing precedes ingestion because that is the order the dashboard enforces for a non-admin
/// member: a granted user may not ingest into an unpaired stream (`enforce_ingest_scope`).
async fn paired_flow_track(app: &Router, db: &DatabaseConnection, admin: &str) -> Flow {
    let track = tracks::onboard_sensor_flow_track(app, admin).await;
    let (completed, failed) = settled_jobs(db, "deployment_create", 1, 60).await;
    assert_eq!(
        (completed, failed),
        (1, 0),
        "opening the track's deployment enqueues exactly one reprocess and it succeeds"
    );

    let stream = track.stream_ids[0].clone();

    // Attaching the stream to the track's instrument is what makes pairing reuse the open
    // deployment instead of failing to claim an occupied slot.
    let sensor_hint = track.sensor_id.clone().expect("track B carries a sensor");
    e2e::link_stream_sensor(app, admin, &stream, &sensor_hint).await;

    let (status, body) = crate::common::post_json_with_token(
        app,
        &format!("/api/streams/{stream}/pair"),
        &json!({ "site_parameter_id": track.site_parameter_ids[0] }),
        admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "pair the registered stream to the slot ({status}): {body}"
    );

    let sensor = track.sensor_id.clone().expect("track B carries a sensor");
    let deployment = track.deployment_id.clone().expect("track B carries a deployment");
    let parameter = track.parameter_id("TrkFlowDO").to_string();
    let parameter_code = track.parameters[0].0.clone();
    Flow {
        track,
        sensor,
        deployment,
        stream,
        parameter,
        parameter_code,
    }
}

/// One sync cycle through `POST /api/ingest`, the write path a river-level member owns.
async fn ingest_cycle(app: &Router, jwt: &str, stream: &str, cycle: usize) {
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/ingest",
        &json!({ "stream_id": stream, "readings": tracks::flow_cycle_readings(cycle) }),
        jwt,
    )
    .await;
    assert_eq!(status, 200, "ingest cycle {cycle} ({status}): {body}");
    assert_eq!(
        body["inserted"].as_u64(),
        Some(tracks::FLOW_READINGS_PER_CYCLE as u64),
        "every reading of cycle {cycle} lands: {body}"
    );
    assert_eq!(
        body["paired"].as_bool(),
        Some(true),
        "the stream is paired, so the cycle is attributed at write time: {body}"
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn deploy_dialog_suggestions_then_redeploy_binds_the_slot() {
    if !kc::require_keycloak_or_skip("deploy_dialog_suggestions").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let flow = paired_flow_track(&app, &db, &admin).await;
    let manager = actor(&db, "manager1", "riverdata-manager", &flow.track.project_id).await;
    let river = actor(&db, "river1", "riverdata-river", &flow.track.project_id).await;
    let intern = actor(&db, "intern1", "riverdata-intern", &flow.track.project_id).await;

    // Cycle 1 starts at 00:00:10, clear of both the deployment start and the recall instant, so
    // `first_reading` cannot be satisfied by echoing either one back.
    ingest_cycle(&app, &river, &flow.stream, 1).await;

    let suggestions = format!("/api/sensors/{}/adopt_suggestions", flow.sensor);
    let (status, before) = crate::common::get_json_with_token(&app, &suggestions, &intern).await;
    assert_eq!(
        status, 200,
        "an intern may read deploy suggestions ({status}): {before}"
    );
    assert_eq!(
        ts(&before, "end_of_last_deployment"),
        Some(flow_dt(0)),
        "with the deployment still open the timeline ends at its start: {before}"
    );

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/sensor_deployments/{}", flow.deployment),
        &json!({ "deployed_until": flow_at(7200) }),
        &manager,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "a manager recalls the sensor ({status}): {body}"
    );
    assert_eq!(
        settled_jobs(&db, "deployment_update", 1, 60).await,
        (1, 0),
        "the recall enqueues one reprocess and it succeeds"
    );

    let (status, after) = crate::common::get_json_with_token(&app, &suggestions, &intern).await;
    assert_eq!(status, 200, "suggestions after the recall ({status}): {after}");
    assert_eq!(
        ts(&after, "end_of_last_deployment"),
        Some(flow_dt(7200)),
        "the recall instant becomes the suggested restart point: {after}"
    );
    assert_eq!(
        ts(&after, "first_reading"),
        Some(flow_dt(10)),
        "first_reading is the sensor's earliest reading, not its deployment start ({}) \
         nor the recall instant ({}): {after}",
        flow_at(0),
        flow_at(7200)
    );
    let now = ts(&after, "now")
        .unwrap_or_else(|| panic!("the 'now' suggestion must be an RFC3339 instant: {after}"));
    assert!(
        (Utc::now() - now).num_seconds().abs() < 300,
        "'now' is the server clock at request time, not a stored date: {now} vs {}",
        Utc::now()
    );

    kc::ensure_realm_user("norole", "norole", &[]).await;
    let norole = kc::get_keycloak_jwt("norole", "norole").await;
    let (status, body) = crate::common::get_with_token(&app, &suggestions, &norole).await;
    assert_eq!(
        status, 403,
        "a valid login without a riverdata role is not membership: {body}"
    );

    // The deploy dialog's create. `parameter_id` is a required field of the create model
    // (`deployments/model.rs`, no `on_create`), so the slot is named explicitly.
    let (status, redeploy) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sensor_deployments",
        &json!({
            "sensor_id": flow.sensor,
            "site_id": flow.track.site_id,
            "parameter_id": flow.parameter,
            "deployed_from": flow_at(10800),
            "deployment_type": "permanent",
        }),
        &manager,
    )
    .await;
    assert_eq!(
        status, 201,
        "a manager redeploys the sensor to the slot it was recalled from ({status}): {redeploy}"
    );
    assert!(
        redeploy["deployed_until"].is_null(),
        "a fresh deployment is open-ended: {redeploy}"
    );
    assert_eq!(
        redeploy["parameter_id"].as_str(),
        Some(flow.parameter.as_str()),
        "the deployment binds the sensor to the slot's parameter: {redeploy}"
    );
    assert_eq!(
        settled_jobs(&db, "deployment_create", 2, 60).await,
        (2, 0),
        "the redeploy enqueues a second reprocess and both succeed"
    );

    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM sensor_deployments \
                 WHERE sensor_id = '{}' AND deployed_until IS NULL",
                flow.sensor
            )
        )
        .await,
        1,
        "exactly one open deployment holds the slot after the redeploy"
    );
    assert_eq!(
        deployment_window(&db, &flow.deployment).await.1,
        Some(flow_dt(7200)),
        "the recalled deployment keeps the close date the operator set"
    );

    let rows = slot_readings(&db, &flow.parameter).await;
    assert_eq!(
        rows.len(),
        tracks::FLOW_READINGS_PER_CYCLE,
        "the ingested cycle is intact: {rows:?}"
    );
    for r in &rows {
        assert_eq!(
            r.deployment_id,
            Some(uid(&flow.deployment)),
            "the reading at {} stays with the deployment whose window covers it; the new \
             [{}, open) window claims no history",
            r.time,
            flow_at(10800)
        );
        assert_eq!(
            r.site_id,
            Some(uid(&flow.track.site_id)),
            "and it stays attributed to the site: {:?}",
            r.time
        );
    }
}

#[tokio::test]
#[serial]
async fn recall_then_reopen_via_edit_dates_restores_attribution() {
    if !kc::require_keycloak_or_skip("recall_then_reopen").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let flow = paired_flow_track(&app, &db, &admin).await;
    let manager = actor(&db, "manager1", "riverdata-manager", &flow.track.project_id).await;
    let river = actor(&db, "river1", "riverdata-river", &flow.track.project_id).await;

    ingest_cycle(&app, &river, &flow.stream, 0).await;
    ingest_cycle(&app, &river, &flow.stream, 1).await;
    let total = 2 * tracks::FLOW_READINGS_PER_CYCLE;

    let identity = e2e::identity_calibration_id(&app, &admin, &flow.sensor).await;
    let identity = uid(
        &identity.expect("pairing gives the sensor its identity calibration"),
    );

    let deployment_path = format!("/api/sensor_deployments/{}", flow.deployment);
    let recall = json!({ "deployed_until": flow_at(10) });

    let (status, body) =
        crate::common::put_json_with_token(&app, &deployment_path, &recall, &river).await;
    assert_eq!(
        status, 403,
        "a river member curates data but does not move sensors: {body}"
    );

    let (status, body) =
        crate::common::put_json_with_token(&app, &deployment_path, &recall, &manager).await;
    assert!(
        (200..300).contains(&status),
        "a manager recalls the sensor ({status}): {body}"
    );
    assert_eq!(
        settled_jobs(&db, "deployment_update", 1, 60).await,
        (1, 0),
        "the recall enqueues one reprocess and it succeeds"
    );

    assert_eq!(
        deployment_window(&db, &flow.deployment).await.1,
        Some(flow_dt(10)),
        "the window closes at the instant the operator typed"
    );
    let rows = slot_readings(&db, &flow.parameter).await;
    assert_eq!(rows.len(), total, "both cycles are still stored: {rows:?}");
    for r in &rows {
        if r.time < flow_dt(10) {
            assert_eq!(
                r.deployment_id,
                Some(uid(&flow.deployment)),
                "the reading at {} predates the recall and keeps its deployment",
                r.time
            );
            assert_eq!(
                r.site_id,
                Some(uid(&flow.track.site_id)),
                "and keeps its site: {}",
                r.time
            );
        } else {
            assert_eq!(
                r.deployment_id, None,
                "the reading at {} was logged with the sensor pulled out",
                r.time
            );
            assert_eq!(
                r.site_id, None,
                "so it belongs to no site: {}",
                r.time
            );
        }
    }

    // The edit dialog sends both dates, an emptied until-field as JSON null.
    let (status, body) = crate::common::put_json_with_token(
        &app,
        &deployment_path,
        &json!({ "deployed_from": flow_at(0), "deployed_until": serde_json::Value::Null }),
        &manager,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "a manager re-opens the deployment ({status}): {body}"
    );
    assert_eq!(
        settled_jobs(&db, "deployment_update", 2, 60).await,
        (2, 0),
        "one reprocess per edit, both successful"
    );

    let (from, until) = deployment_window(&db, &flow.deployment).await;
    assert_eq!(from, flow_dt(0), "the start is the one the dialog sent back");
    assert_eq!(
        until, None,
        "an explicit null re-opens the deployment, and the window recompute must not re-close it"
    );

    let rows = slot_readings(&db, &flow.parameter).await;
    assert_eq!(rows.len(), total, "no reading was lost: {rows:?}");
    for r in &rows {
        assert_eq!(
            r.site_id,
            Some(uid(&flow.track.site_id)),
            "the reading at {} is back at the site the re-opened window covers",
            r.time
        );
        assert_eq!(
            r.deployment_id,
            Some(uid(&flow.deployment)),
            "and back on the deployment: {}",
            r.time
        );
        assert_eq!(
            r.sensor_id,
            Some(uid(&flow.sensor)),
            "ownership never moved: {}",
            r.time
        );
        assert_eq!(
            r.calibration_id,
            Some(identity),
            "the deployment round trip re-derives attribution without disturbing the calibration \
             window: {}",
            r.time
        );
        assert_eq!(
            r.calibrated_value,
            Some(r.raw_value),
            "the identity curve leaves the value alone: {}",
            r.time
        );
    }
    let raws: Vec<f64> = rows.iter().map(|r| r.raw_value).collect();
    let expected: Vec<f64> = (0..total).map(|i| tracks::BAND_FLOW.0 + i as f64).collect();
    assert_eq!(raws, expected, "the measured values survive the round trip");
}

#[tokio::test]
#[serial]
async fn backdate_deployed_from_claims_unattributed_slot_history() {
    if !kc::require_keycloak_or_skip("backdate_deployed_from").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let flow = paired_flow_track(&app, &db, &admin).await;
    let manager = actor(&db, "manager1", "riverdata-manager", &flow.track.project_id).await;
    let river = actor(&db, "river1", "riverdata-river", &flow.track.project_id).await;
    let intern = actor(&db, "intern1", "riverdata-intern", &flow.track.project_id).await;

    ingest_cycle(&app, &river, &flow.stream, 0).await;

    // A historical upload into the same slot, predating the deployment. No deployment window covers
    // it, so the importer leaves it unowned: the state the sensor page's backdate banner detects.
    let (status, imported) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &json!({
            "site": flow.track.site_id,
            "csv": history_csv(&flow.parameter_code),
            "dry_run": false,
        }),
        &river,
    )
    .await;
    assert_eq!(
        status, 200,
        "a river member imports the historical CSV ({status}): {imported}"
    );
    assert_eq!(
        imported["row_count"].as_u64(),
        Some(2),
        "both history rows parse: {imported}"
    );
    assert_eq!(
        settled_jobs(&db, "csv_import", 1, 60).await,
        (1, 0),
        "the staged import job moves the rows into readings"
    );

    let orphans = format!(
        "SELECT count(*) AS c FROM readings WHERE parameter_id = '{}' AND sensor_id IS NULL",
        flow.parameter
    );
    assert_eq!(
        count(&db, &orphans).await,
        2,
        "the imported history lands attributed to the slot but to no sensor"
    );

    let series = format!("/api/sensors/{}/readings", flow.sensor);
    let (status, before) = crate::common::get_json_with_token(&app, &series, &intern).await;
    assert_eq!(status, 200, "an intern may read the sensor series ({status}): {before}");
    assert_eq!(
        ts(&before, "data_start"),
        Some(flow_dt(0)),
        "data_start sees only what the sensor already owns: {before}"
    );
    assert_eq!(
        ts(&before, "slot_data_start"),
        Some(dt(HISTORY_START)),
        "slot_data_start sees the whole slot, which is why the backdate button reads it \
         rather than data_start: {before}"
    );
    assert_eq!(
        before["times"].as_array().map(Vec::len),
        Some(tracks::FLOW_READINGS_PER_CYCLE),
        "the unowned history is invisible to the per-sensor series: {before}"
    );

    let target = before["slot_data_start"]
        .as_str()
        .expect("slot_data_start drives the backdate")
        .to_string();
    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/sensor_deployments/{}", flow.deployment),
        &json!({ "deployed_from": target }),
        &manager,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "a manager backdates the deployment to the slot's first reading ({status}): {body}"
    );
    assert_eq!(
        settled_jobs(&db, "deployment_update", 1, 60).await,
        (1, 0),
        "the backdate enqueues one reprocess and it succeeds"
    );

    assert_eq!(
        deployment_window(&db, &flow.deployment).await.0,
        dt(HISTORY_START),
        "the deployment now starts where the slot's data starts"
    );

    let rows = slot_readings(&db, &flow.parameter).await;
    assert_eq!(
        rows.len(),
        tracks::FLOW_READINGS_PER_CYCLE + 2,
        "the imported history and the ingested cycle share the slot: {rows:?}"
    );
    for r in &rows {
        assert_eq!(
            r.sensor_id,
            Some(uid(&flow.sensor)),
            "the backdated window claims the reading at {}",
            r.time
        );
        assert_eq!(
            r.deployment_id,
            Some(uid(&flow.deployment)),
            "and stamps the deployment on it: {}",
            r.time
        );
        assert_eq!(
            r.site_id,
            Some(uid(&flow.track.site_id)),
            "and the site: {}",
            r.time
        );
        assert!(
            r.calibration_id.is_some(),
            "reprocessing extends calibration coverage over the claimed history: {}",
            r.time
        );
        assert_eq!(
            r.calibrated_value,
            Some(r.raw_value),
            "the sensor's only curve is the identity, so values are unchanged: {}",
            r.time
        );
    }

    let (status, after) = crate::common::get_json_with_token(&app, &series, &intern).await;
    assert_eq!(status, 200, "sensor series after the backdate ({status}): {after}");
    assert_eq!(
        ts(&after, "data_start"),
        Some(dt(HISTORY_START)),
        "the sensor's own extent now reaches back over the claimed history: {after}"
    );
    assert_eq!(
        after["times"].as_array().map(Vec::len),
        Some(tracks::FLOW_READINGS_PER_CYCLE + 2),
        "and the series carries every claimed point: {after}"
    );
}

#[tokio::test]
#[serial]
async fn calibration_candidates_then_backfill_calibrations() {
    if !kc::require_keycloak_or_skip("calibration_candidates").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let flow = paired_flow_track(&app, &db, &admin).await;
    let river = actor(&db, "river1", "riverdata-river", &flow.track.project_id).await;
    let intern = actor(&db, "intern1", "riverdata-intern", &flow.track.project_id).await;

    ingest_cycle(&app, &river, &flow.stream, 0).await;
    let ingested = tracks::FLOW_READINGS_PER_CYCLE as u64;

    // The identity curve pairing created starts at pairing time, so back-dated readings sit before
    // every calibration window and carry no calibration_id: the uncovered state the sensors list
    // reports and offers to fix.
    let (status, candidates) =
        crate::common::get_json_with_token(&app, "/api/actions/calibration_candidates", &intern).await;
    assert_eq!(
        status, 200,
        "an intern may read the calibration candidates ({status}): {candidates}"
    );
    assert_eq!(
        candidates["total_candidates"].as_u64(),
        Some(1),
        "the one sensor with uncovered readings is reported: {candidates}"
    );
    assert_eq!(
        candidates["total_uncalibrated"].as_u64(),
        Some(ingested),
        "every uncovered reading is counted: {candidates}"
    );
    let candidate = candidates["candidates"][0].clone();
    assert_eq!(
        candidate["sensor_id"].as_str(),
        Some(flow.sensor.as_str()),
        "the candidate names the track's sensor: {candidates}"
    );
    assert_eq!(
        candidate["uncalibrated_count"].as_u64(),
        Some(ingested),
        "with its own uncovered count: {candidates}"
    );
    assert_eq!(
        ts(&candidate, "target_from"),
        Some(flow_dt(0)),
        "the earliest uncovered reading is the backfill target: {candidates}"
    );
    assert_eq!(
        candidate["is_identity"].as_bool(),
        Some(true),
        "the sensor's earliest curve is the identity, so coverage can be extended in place: \
         {candidates}"
    );
    let earliest = ts(&candidate, "earliest_calibration_from")
        .unwrap_or_else(|| panic!("the candidate reports the first curve's start: {candidates}"));
    assert!(
        earliest > flow_dt(0),
        "the gap is real: the only curve starts at {earliest}, after the data at {}",
        flow_at(0)
    );

    let backfill = "/api/actions/backfill_calibrations";
    let (status, body) =
        crate::common::post_json_with_token(&app, backfill, &json!({}), &river).await;
    assert_eq!(
        status, 400,
        "a backfill naming no sensor is refused even while a candidate exists: {body}"
    );

    let selector = json!({ "sensor_id": flow.sensor });
    let (status, body) =
        crate::common::post_json_with_token(&app, backfill, &selector, &intern).await;
    assert_eq!(
        status, 403,
        "backfilling calibrations rewrites readings, which an intern may not do: {body}"
    );

    let (status, run) =
        crate::common::post_json_parse_with_token(&app, backfill, &selector, &river).await;
    assert_eq!(status, 200, "a river member runs the backfill ({status}): {run}");
    assert_eq!(
        run["sensors_updated"].as_u64(),
        Some(1),
        "one sensor is covered: {run}"
    );
    assert_eq!(
        run["estimated_readings"].as_u64(),
        Some(ingested),
        "over its uncovered readings: {run}"
    );
    let job_id = run["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the backfill returns a tracked job: {run}"))
        .to_string();
    assert_eq!(
        e2e::poll_job(&app, &admin, &job_id, 60).await,
        "completed",
        "the backfill job runs to completion"
    );

    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM sensor_calibrations WHERE sensor_id = '{}'",
                flow.sensor
            )
        )
        .await,
        1,
        "the existing identity curve is extended in place, not duplicated"
    );
    let curve = earliest_curve(&db, &flow.sensor).await;
    assert!(
        (curve.slope - 1.0).abs() < f64::EPSILON && curve.intercept.abs() < f64::EPSILON,
        "coverage is filled with an identity curve, never invented coefficients: \
         slope {} intercept {}",
        curve.slope,
        curve.intercept
    );
    assert_eq!(
        curve.valid_from,
        flow_dt(0),
        "the curve now starts at the earliest reading it must cover"
    );
    assert_eq!(
        curve.valid_until, None,
        "it is the sensor's only window, so nothing chains after it"
    );

    let rows = slot_readings(&db, &flow.parameter).await;
    assert_eq!(rows.len(), tracks::FLOW_READINGS_PER_CYCLE, "the cycle is intact: {rows:?}");
    for r in &rows {
        assert_eq!(
            r.calibration_id,
            Some(curve.id),
            "the reading at {} now resolves to the extended curve",
            r.time
        );
        assert_eq!(
            r.calibrated_value,
            Some(r.raw_value),
            "and its identity-calibrated value equals its raw value: {}",
            r.time
        );
    }

    let (status, cleared) =
        crate::common::get_json_with_token(&app, "/api/actions/calibration_candidates", &intern).await;
    assert_eq!(status, 200, "candidates after the backfill ({status}): {cleared}");
    assert_eq!(
        cleared["total_candidates"].as_u64(),
        Some(0),
        "nothing is left to backfill: {cleared}"
    );
    assert_eq!(
        cleared["total_uncalibrated"].as_u64(),
        Some(0),
        "and no reading is left uncovered: {cleared}"
    );
}

#[tokio::test]
#[serial]
async fn recalculate_action_rewrites_the_calibration_window() {
    if !kc::require_keycloak_or_skip("recalculate_action").await {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let flow = paired_flow_track(&app, &db, &admin).await;
    let manager = actor(&db, "manager1", "riverdata-manager", &flow.track.project_id).await;
    let river = actor(&db, "river1", "riverdata-river", &flow.track.project_id).await;

    let (status, created) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sensor_calibrations",
        &json!({
            "sensor_id": flow.sensor,
            "slope": 2.0,
            "intercept": 5.0,
            "valid_from": "2025-06-01T00:00:00Z",
        }),
        &manager,
    )
    .await;
    assert_eq!(
        status, 201,
        "a manager records the curve the lab measured ({status}): {created}"
    );
    let curve = uid(&e2e::id_of(&created));
    assert_eq!(
        settled_jobs(&db, "calibration_create", 1, 60).await,
        (1, 0),
        "recording a curve enqueues one reprocess and it succeeds"
    );

    ingest_cycle(&app, &river, &flow.stream, 0).await;

    // Ingest stamps the covering curve but writes the raw value through unchanged
    // (`readings/ingest.rs`: "calibrated_value is written as identity (raw) here; reprocess
    // refines it"), which is the drift the sensor page's recalculate button exists to clear.
    let stale = slot_readings(&db, &flow.parameter).await;
    assert_eq!(
        stale.len(),
        tracks::FLOW_READINGS_PER_CYCLE,
        "the cycle landed: {stale:?}"
    );
    for r in &stale {
        assert_eq!(
            r.calibration_id,
            Some(curve),
            "the reading at {} resolves to the curve covering its time",
            r.time
        );
        assert_eq!(
            r.calibrated_value,
            Some(r.raw_value),
            "but arrives carrying its raw value: {}",
            r.time
        );
    }

    let recalculate = format!("/api/actions/sensor_calibrations/{curve}/recalculate");
    let (status, body) =
        crate::common::post_json_with_token(&app, &recalculate, &json!({}), &river).await;
    assert_eq!(
        status, 403,
        "recalculating a curve is a sensor-management action: {body}"
    );

    let (status, run) =
        crate::common::post_json_parse_with_token(&app, &recalculate, &json!({}), &manager).await;
    assert_eq!(status, 200, "a manager recalculates the curve ({status}): {run}");
    let job_id = run["job_id"]
        .as_str()
        .unwrap_or_else(|| panic!("recalculate returns a tracked job: {run}"))
        .to_string();
    assert_eq!(
        e2e::poll_job(&app, &manager, &job_id, 60).await,
        "completed",
        "the recalculate job runs to completion"
    );

    let (status, job) = crate::common::get_json_with_token(
        &app,
        &format!("/api/reprocessing_jobs/{job_id}"),
        &manager,
    )
    .await;
    assert_eq!(status, 200, "the job is visible to the operator who ran it ({status}): {job}");
    assert_eq!(
        job["trigger_type"].as_str(),
        Some("calibration_recalculate"),
        "the job records what triggered it: {job}"
    );
    assert_eq!(
        job["sensor_id"].as_str(),
        Some(flow.sensor.as_str()),
        "and which sensor it reprocessed: {job}"
    );

    let rows = slot_readings(&db, &flow.parameter).await;
    assert_eq!(rows.len(), tracks::FLOW_READINGS_PER_CYCLE, "no reading was lost: {rows:?}");
    for r in &rows {
        assert_eq!(
            r.calibrated_value,
            Some(2.0 * r.raw_value + 5.0),
            "the reading at {} carries the curve applied to its raw value {}",
            r.time,
            r.raw_value
        );
        assert_eq!(
            r.calibration_id,
            Some(curve),
            "against the same curve it already resolved to: {}",
            r.time
        );
        assert_eq!(
            r.sensor_id,
            Some(uid(&flow.sensor)),
            "recalculating coefficients disturbs no ownership: {}",
            r.time
        );
        assert_eq!(
            r.deployment_id,
            Some(uid(&flow.deployment)),
            "nor the deployment: {}",
            r.time
        );
        assert_eq!(
            r.site_id,
            Some(uid(&flow.track.site_id)),
            "nor the site: {}",
            r.time
        );
    }

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/actions/sensor_calibrations/{}/recalculate", Uuid::new_v4()),
        &json!({}),
        &manager,
    )
    .await;
    assert_eq!(status, 404, "recalculating a curve that does not exist: {body}");
}
