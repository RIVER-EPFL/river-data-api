//! HTTP-driven workflow helpers for end-to-end tests.
//!
//! Each helper creates an entity through its real endpoint (mirroring the payloads proven by
//! `public_workflow_e2e_test.rs`) and returns its id, so an `e2e_*` test reads as a sequence of
//! user actions. `poll_job` waits on tracked reprocessing jobs; `field_for`/`values_for` pull
//! numeric arrays out of readings/aggregate responses for 1:1 assertions.

use axum::Router;
use serde_json::json;
use std::time::{Duration, Instant};

/// Refresh the hourly continuous aggregate from `since` to now.
///
/// The production refresh window is `[since, NOW()]` (`common/sync_state.rs`), so a fixture dated
/// in the future is never materialised. Keep fixture times in the past.
pub async fn refresh_hourly(
    db: &sea_orm::DatabaseConnection,
    since: chrono::DateTime<chrono::Utc>,
) {
    use sea_orm::{ConnectionTrait, Statement};
    db.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "CALL refresh_continuous_aggregate('readings_hourly', '{}'::timestamptz, NOW())",
            since.to_rfc3339()
        ),
    ))
    .await
    .expect("refresh readings_hourly");
}

/// The hourly bucket a (site, parameter) resolves at `at`, as `(mean, count)`, or `None` when the
/// bucket holds no rows.
///
/// Read through `/api/sites/{id}/aggregates/hourly`, so the sensor collapse a slot served by more
/// than one instrument needs is whatever production serves rather than a second copy of the rule.
pub async fn hourly_bucket(
    app: &Router,
    token: &str,
    site_id: &str,
    parameter_id: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> Option<(f64, i64)> {
    use chrono::{Timelike, Utc};
    let bucket: chrono::DateTime<Utc> = at
        .with_minute(0)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .expect("truncate to the hour");
    let stamp = bucket.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let (_s, resp) = super::get_json_with_token(
        app,
        &format!(
            "/api/sites/{site_id}/aggregates/hourly\
             ?start={stamp}&end={stamp}&parameter_ids={parameter_id}"
        ),
        token,
    )
    .await;
    let entry = resp["parameters"]
        .as_array()?
        .iter()
        .find(|p| p["parameter_id"] == parameter_id || p["code"] == parameter_id)?;
    let mean = entry["avg"].as_array()?.first()?.as_f64()?;
    let count = entry["count"].as_array()?.first()?.as_i64()?;
    (count > 0).then_some((mean, count))
}

/// The first column of a single-row COUNT query.
pub async fn count(db: &sea_orm::DatabaseConnection, sql: &str) -> i64 {
    use sea_orm::{ConnectionTrait, Statement};
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("count query failed: {e}\n{sql}"))
    .unwrap_or_else(|| panic!("count query returned no row: {sql}"))
    .try_get_by_index::<i64>(0)
    .unwrap_or_else(|e| panic!("count query has no i64 first column: {e}\n{sql}"))
}

/// The first column of a single-row query, as text.
pub async fn scalar(db: &sea_orm::DatabaseConnection, sql: &str) -> String {
    use sea_orm::{ConnectionTrait, Statement};
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("scalar query failed: {e}\n{sql}"))
    .unwrap_or_else(|| panic!("scalar query returned no row: {sql}"))
    .try_get_by_index::<String>(0)
    .unwrap_or_else(|e| panic!("scalar query has no text first column: {e}\n{sql}"))
}

/// Percent-encode a CrudCrate `filter` value for use in a query string.
pub fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The sensor's earliest recorded calibration, if it has one.
///
/// Returns None for a sensor nobody has entered a curve for, which is an ordinary state: no path
/// creates a calibration on a sensor's behalf, so a curve exists only where an operator put one.
pub async fn first_calibration_id(app: &Router, token: &str, sensor_id: &str) -> Option<String> {
    let filter = percent_encode(&format!(r#"{{"sensor_id":"{sensor_id}"}}"#));
    let (status, body) = super::get_json_with_token(
        app,
        &format!("/api/sensor_calibrations?filter={filter}"),
        token,
    )
    .await;
    assert_eq!(status, 200, "list calibrations for {sensor_id}: {body}");
    body.as_array()
        .and_then(|a| a.first())
        .and_then(|c| c["id"].as_str())
        .map(str::to_string)
}

/// Extract the `id` field from a created-entity response.
pub fn id_of(json: &serde_json::Value) -> String {
    json["id"]
        .as_str()
        .unwrap_or_else(|| panic!("created entity must have an id: {json}"))
        .to_string()
}

/// Poll a reprocessing job to a terminal status and return it. A deadline elapsing is a failure of
/// the test machine, not a status, so it panics rather than reporting whatever was last observed.
pub async fn poll_job(app: &Router, token: &str, job_id: &str, max_secs: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(max_secs);
    loop {
        let (_s, job) =
            super::get_json_with_token(app, &format!("/api/reprocessing_jobs/{job_id}"), token)
                .await;
        let status = job["status"].as_str().unwrap_or("").to_string();
        if status == "completed" || status == "failed" {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "job {job_id} still {status} after {max_secs}s"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Wait for all reprocessing jobs of a given `trigger_type` to reach a terminal state. Returns true
/// if at least one job ran and none failed, false if one failed, and panics if the deadline elapses
/// with jobs still running. For background jobs whose id isn't returned by the triggering request
/// (e.g. `derived_assignment`, which has a NULL sensor_id).
pub async fn wait_for_jobs_by_trigger(
    db: &sea_orm::DatabaseConnection,
    trigger_type: &str,
    timeout_secs: u64,
) -> bool {
    use sea_orm::{ConnectionTrait, Statement};
    let start = Instant::now();
    loop {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT \
                   COUNT(*) FILTER (WHERE status IN ('queued','pending','running','retrying')) AS active, \
                   COUNT(*) FILTER (WHERE status = 'failed') AS failed, \
                   COUNT(*) AS total \
                 FROM reprocessing_jobs WHERE trigger_type = $1",
                [trigger_type.into()],
            ))
            .await
            .expect("query reprocessing_jobs")
            .expect("count row");
        let active: i64 = row.try_get("", "active").unwrap();
        let failed: i64 = row.try_get("", "failed").unwrap();
        let total: i64 = row.try_get("", "total").unwrap();
        if total > 0 && active == 0 {
            return failed == 0;
        }
        assert!(
            start.elapsed().as_secs() <= timeout_secs,
            "{trigger_type}: {active} of {total} jobs still running after {timeout_secs}s"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A numeric array from a readings (`values`) or aggregate (`avg`/`min`/`max`/`count`) response.
/// `key` matches a parameter by `code`, `name`, `display_name`, or `parameter_id`, the
/// authenticated readings group by the site_parameter name while the public API exposes `code`
/// (short code) and `name` (human label), so matching on `parameter_id` or `code` is the stable
/// choice across both.
pub fn field_for(resp: &serde_json::Value, key: &str, field: &str) -> Vec<f64> {
    resp["parameters"]
        .as_array()
        .unwrap_or_else(|| panic!("no 'parameters' array in response: {resp}"))
        .iter()
        .find(|p| {
            p["code"] == key
                || p["name"] == key
                || p["display_name"] == key
                || p["parameter_id"] == key
        })
        .unwrap_or_else(|| panic!("parameter {key} missing in {resp}"))[field]
        .as_array()
        .unwrap_or_else(|| panic!("'{field}' not an array for {key}"))
        .iter()
        .map(|v| v.as_f64().unwrap_or(f64::NAN))
        .collect()
}

pub fn values_for(resp: &serde_json::Value, key: &str) -> Vec<f64> {
    field_for(resp, key, "values")
}

async fn create(app: &Router, token: &str, path: &str, body: serde_json::Value) -> String {
    let (status, json) = super::post_json_parse_with_token(app, path, &body, token).await;
    assert!(
        (200..300).contains(&status),
        "create {path} ({status}): {json}"
    );
    id_of(&json)
}

pub async fn create_project(
    app: &Router,
    token: &str,
    name: &str,
    code: &str,
    public: bool,
) -> String {
    create(
        app,
        token,
        "/api/projects",
        json!({ "name": name, "description": "e2e", "is_public": public, "public_code": code }),
    )
    .await
}

pub async fn create_site(
    app: &Router,
    token: &str,
    project_id: &str,
    name: &str,
    code: &str,
) -> String {
    create(
        app,
        token,
        "/api/sites",
        json!({ "name": name, "project_id": project_id, "latitude": 46.0, "longitude": 7.0, "public_code": code }),
    )
    .await
}

pub async fn create_parameter(
    app: &Router,
    token: &str,
    code: &str,
    name: &str,
    units: &str,
) -> String {
    create(
        app,
        token,
        "/api/parameters",
        json!({ "code": code, "name": name, "default_units": units, "category": "measurement", "aliases": [] }),
    )
    .await
}

/// Assign a parameter to a site with ONLY the required fields, exercises the `on_create` defaults
/// and the server-side `name` backfill.
pub async fn assign_site_parameter_minimal(
    app: &Router,
    token: &str,
    site_id: &str,
    parameter_id: &str,
) -> String {
    create(
        app,
        token,
        "/api/site_parameters",
        json!({ "site_id": site_id, "parameter_id": parameter_id }),
    )
    .await
}

/// The slots a site carries, declared the way the portal declares them: a parameter group holding
/// each parameter, applied to the site. A grab save refuses a parameter the site does not carry
/// (Q98), so a story that saves tool output declares the outputs here first.
pub async fn declare_site_slots(
    db: &sea_orm::DatabaseConnection,
    app: &Router,
    token: &str,
    site_id: &str,
    code: &str,
    members: &[&str],
) -> String {
    let group_id = uuid::Uuid::new_v4().to_string();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO parameter_groups (id, code, label, ordinal) \
             VALUES ('{group_id}', '{code}', '{code}', 1)"
        ),
    )
    .await;
    for (ordinal, parameter) in members.iter().enumerate() {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal) \
                 VALUES (gen_random_uuid(), '{group_id}', '{parameter}', {})",
                ordinal + 1
            ),
        )
        .await;
    }
    let (status, applied) = crate::common::post_json_with_token(
        app,
        &format!("/api/sites/{site_id}/parameter_groups"),
        &json!({ "group_id": group_id }),
        token,
    )
    .await;
    assert_eq!(status, 200, "apply the group: {applied}");
    group_id
}

pub async fn create_sensor(app: &Router, token: &str, _parameter_id: &str, serial: &str) -> String {
    // A sensor is parameter-free; the parameter is bound at deploy time (see `create_deployment`).
    create(
        app,
        token,
        "/api/sensors",
        json!({ "serial_number": serial, "manufacturer": "e2e", "model": "test" }),
    )
    .await
}

/// Attach a registered stream to the sensor that feeds it.
///
/// `POST /streams/register` accepts no sensor field, so the link is set afterwards; without it,
/// pairing mints a second, serial-less, deployment-less sensor for the same feed.
pub async fn link_stream_sensor(app: &Router, token: &str, stream_id: &str, sensor_id: &str) {
    let (status, body) = super::put_json_with_token(
        app,
        &format!("/api/data_streams/{stream_id}"),
        &json!({ "sensor_id": sensor_id }),
        token,
    )
    .await;
    assert_eq!(
        status, 200,
        "attach stream {stream_id} to sensor {sensor_id}: {body}"
    );
}

pub async fn create_deployment(
    app: &Router,
    token: &str,
    sensor_id: &str,
    site_id: &str,
    parameter_id: &str,
    deployed_from: &str,
) -> String {
    create(
        app,
        token,
        "/api/sensor_deployments",
        json!({
            "sensor_id": sensor_id,
            "site_id": site_id,
            "parameter_id": parameter_id,
            "deployed_from": deployed_from,
        }),
    )
    .await
}

/// Enter a calibration curve on a sensor.
pub async fn create_calibration(
    app: &Router,
    token: &str,
    sensor_id: &str,
    parameter_id: &str,
    slope: f64,
    intercept: f64,
    valid_from: &str,
) -> String {
    create(
        app,
        token,
        "/api/sensor_calibrations",
        json!({
            "sensor_id": sensor_id,
            "parameter_id": parameter_id,
            "slope": slope,
            "intercept": intercept,
            "valid_from": valid_from,
        }),
    )
    .await
}

/// Mark a site_parameter public through its update route, as an operator exposes one.
pub async fn set_site_parameter_public(app: &Router, token: &str, sp_id: &str) {
    let (status, body) = crate::common::put_json_with_token(
        app,
        &format!("/api/site_parameters/{sp_id}"),
        &serde_json::json!({ "is_public": true }),
        token,
    )
    .await;
    assert_eq!(status, 200, "mark site_parameter {sp_id} public: {body}");
}

/// Take a calculation out of the calculation set by decommissioning it, or bring it back by
/// recommissioning it, as `admin`. A decommission frees the name, so the way back finds the
/// calculation under the name it was left with.
pub async fn set_live(app: &Router, admin: &str, name: &str, live: bool) {
    let (status, scripts) =
        crate::common::client::get_json_with_token(app, "/api/tool_scripts", admin).await;
    assert_eq!(status, 200, "{scripts}");
    let freed = format!("{name}_decommissioned_");
    let id = scripts
        .as_array()
        .expect("a list")
        .iter()
        .find(|s| {
            let listed = s["name"].as_str().unwrap_or_default();
            if live {
                listed.starts_with(&freed) && !s["decommissioned_at"].is_null()
            } else {
                listed == name
            }
        })
        .and_then(|s| s["id"].as_str())
        .unwrap_or_else(|| panic!("{name} is listed: {scripts}"))
        .to_string();
    let verb = if live { "recommission" } else { "decommission" };
    let (status, body) = crate::common::client::post_json_parse_with_token(
        app,
        &format!("/api/tool_scripts/{id}/{verb}"),
        &json!({ "reason": "staged by the story" }),
        admin,
    )
    .await;
    assert_eq!(status, 200, "{verb} {name}: {body}");
}

/// Author, validate and activate one tool version as `admin`. Runner-backed: validation runs
/// the stored cases.
pub async fn author_tool(
    app: &Router,
    admin: &str,
    name: &str,
    script: &str,
    manifest: serde_json::Value,
    case: serde_json::Value,
) {
    let (status, created) = crate::common::client::post_json_parse_with_token(
        app,
        "/api/tool_scripts",
        &json!({ "name": name, "label": name }),
        admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create {name} ({status}): {created}"
    );
    let script_id = id_of(&created);

    let (status, version) = crate::common::client::post_json_parse_with_token(
        app,
        &format!("/api/tool_scripts/{script_id}/versions"),
        &json!({
            "script": script,
            "manifest": manifest,
            "test_cases": { "cases": [case] },
        }),
        admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "version {name} ({status}): {version}"
    );
    let version_id = id_of(&version["version"]);

    let (status, validated) = crate::common::client::post_json_parse_with_token(
        app,
        &format!("/api/tool_scripts/{script_id}/versions/{version_id}/validate"),
        &json!({}),
        admin,
    )
    .await;
    assert!(
        (200..300).contains(&status) && validated["passed"] == true,
        "validate {name} ({status}): {validated}"
    );

    let (status, activated) = crate::common::client::post_json_parse_with_token(
        app,
        &format!("/api/tool_scripts/{script_id}/versions/{version_id}/activate"),
        &json!({}),
        admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "activate {name} ({status}): {activated}"
    );
}

/// Mint and activate a further version of a tool that already exists, the shape a calculation edit
/// takes. Returns nothing: what changed is which version the calculation activates.
pub async fn revise_tool(
    app: &Router,
    admin: &str,
    name: &str,
    script: &str,
    manifest: serde_json::Value,
    case: serde_json::Value,
) {
    let (status, list) =
        super::get_json_with_token(app, "/api/tool_scripts?page=1&per_page=200", admin).await;
    assert_eq!(status, 200, "list tools ({status}): {list}");
    let script_id = list
        .as_array()
        .expect("tool_scripts list is an array")
        .iter()
        .find(|row| row["name"] == name)
        .map(id_of)
        .unwrap_or_else(|| panic!("a tool named {name} exists: {list}"));

    let (status, version) = crate::common::client::post_json_parse_with_token(
        app,
        &format!("/api/tool_scripts/{script_id}/versions"),
        &json!({
            "script": script,
            "manifest": manifest,
            "test_cases": { "cases": [case] },
        }),
        admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "revise {name} ({status}): {version}"
    );
    let version_id = id_of(&version["version"]);

    let (status, validated) = crate::common::client::post_json_parse_with_token(
        app,
        &format!("/api/tool_scripts/{script_id}/versions/{version_id}/validate"),
        &json!({}),
        admin,
    )
    .await;
    assert!(
        (200..300).contains(&status) && validated["passed"] == true,
        "validate {name} ({status}): {validated}"
    );

    let (status, activated) = crate::common::client::post_json_parse_with_token(
        app,
        &format!("/api/tool_scripts/{script_id}/versions/{version_id}/activate"),
        &json!({}),
        admin,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "activate {name} ({status}): {activated}"
    );
}

/// The open event-audit findings at a site, read from the review queue the dashboard reads.
pub async fn pending_event_findings(
    app: &Router,
    token: &str,
    site_id: &str,
) -> Vec<serde_json::Value> {
    let (status, body) = super::get_json_with_token(
        app,
        "/api/sync/replicate_audit_holds?status=pending&page_size=500",
        token,
    )
    .await;
    assert_eq!(status, 200, "holds list ({status}): {body}");
    body["holds"]
        .as_array()
        .expect("holds array")
        .iter()
        .filter(|h| h["stream_id"].is_null() && h["site_id"] == site_id)
        .cloned()
        .collect()
}

/// A Keycloak fixture user at `role`, granted visibility of `project_id`. Fixture passwords equal
/// the username. The realm-user, grant and JWT steps always travel together.
pub async fn member(
    db: &sea_orm::DatabaseConnection,
    project_id: &str,
    user: &str,
    role: &str,
) -> String {
    use crate::common::keycloak as kc;
    kc::ensure_realm_user(user, user, &[role]).await;
    kc::grant_project(db, &kc::keycloak_user_id(user).await, project_id).await;
    kc::get_keycloak_jwt(user, user).await
}

/// [`member`] for several users at once, in the order given.
pub async fn members(
    db: &sea_orm::DatabaseConnection,
    project_id: &str,
    users: &[(&str, &str)],
) -> Vec<String> {
    let mut jwts = Vec::with_capacity(users.len());
    for (user, role) in users {
        jwts.push(member(db, project_id, user, role).await);
    }
    jwts
}

/// An `/api/ingest` body for one stream, `(rfc3339 time, raw value)` per reading.
#[must_use]
pub fn ingest_body(stream_id: &str, readings: &[(&str, f64)]) -> serde_json::Value {
    json!({
        "stream_id": stream_id,
        "readings": readings
            .iter()
            .map(|(time, value)| json!({ "time": time, "raw_value": value }))
            .collect::<Vec<_>>(),
    })
}

/// Ingest onto one stream and assert every reading landed. Returns the response body.
pub async fn ingest(
    app: &Router,
    jwt: &str,
    stream_id: &str,
    readings: &[(&str, f64)],
) -> serde_json::Value {
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/ingest",
        &ingest_body(stream_id, readings),
        jwt,
    )
    .await;
    assert_eq!(status, 200, "ingest onto stream {stream_id}: {body}");
    assert_eq!(
        body["inserted"].as_u64(),
        Some(readings.len() as u64),
        "every ingested reading lands: {body}"
    );
    body
}

/// One line per tracked job, for a failure message that says what the queue was doing.
pub async fn jobs_summary(db: &sea_orm::DatabaseConnection) -> String {
    use sea_orm::{ConnectionTrait, Statement};
    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT trigger_type, status, COALESCE(error_message, '') AS error              FROM reprocessing_jobs ORDER BY created_at",
        ))
        .await
        .expect("query reprocessing_jobs");
    rows.iter()
        .map(|r| {
            let trigger: String = r.try_get("", "trigger_type").unwrap_or_default();
            let status: String = r.try_get("", "status").unwrap_or_default();
            let error: String = r.try_get("", "error").unwrap_or_default();
            if error.is_empty() {
                format!("{trigger}={status}")
            } else {
                format!("{trigger}={status} ({error})")
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Wait until no job of `trigger_type` is active and at least `expected` have settled, returning
/// `(completed, failed)`. A failing job returns to `queued` with a retry delay rather than to
/// `failed`, so the deadline elapsing means the retry budget outlived the test.
pub async fn settled_jobs(
    db: &sea_orm::DatabaseConnection,
    trigger_type: &str,
    expected: i64,
    timeout_secs: u64,
) -> (i64, i64) {
    use sea_orm::{ConnectionTrait, Statement};
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
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
        assert!(
            Instant::now() < deadline,
            "{trigger_type}: {completed} completed, {failed} failed, {active} active after \
             {timeout_secs}s: {}",
            jobs_summary(db).await
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Wait for every tracked job to reach a terminal state, whatever its trigger.
pub async fn drain_jobs(db: &sea_orm::DatabaseConnection, max_secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(max_secs);
    loop {
        let active = count(
            db,
            "SELECT count(*) FROM reprocessing_jobs \
             WHERE status IN ('pending', 'queued', 'running', 'retrying')",
        )
        .await;
        if active == 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "tracked jobs have not settled after {max_secs}s: {}",
            jobs_summary(db).await
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll the newest job of `trigger_type` until it is terminal, returning its status and a summary.
pub async fn await_job(
    db: &sea_orm::DatabaseConnection,
    trigger_type: &str,
    max_secs: u64,
) -> (String, String) {
    use sea_orm::{ConnectionTrait, Statement};
    let deadline = Instant::now() + Duration::from_secs(max_secs);
    loop {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT status FROM reprocessing_jobs WHERE trigger_type = $1 \
                 ORDER BY created_at DESC LIMIT 1",
                [trigger_type.into()],
            ))
            .await
            .expect("job lookup failed");
        let status: String = row
            .map(|r| r.try_get("", "status").unwrap_or_default())
            .unwrap_or_else(|| "missing".to_string());
        if status == "completed" || status == "failed" {
            return (status, jobs_summary(db).await);
        }
        assert!(
            Instant::now() < deadline,
            "{trigger_type} job still {status} after {max_secs}s: {}",
            jobs_summary(db).await
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Float equality at the tolerance stored values are asserted to across these stories.
pub fn assert_close(actual: f64, expected: f64, what: &str) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "{what}: expected {expected}, got {actual}"
    );
}

/// A global parameter plus its slot at `site_id`, for slots a track does not provision itself.
pub async fn provision_slot(
    app: &Router,
    admin: &str,
    site_id: &str,
    code: &str,
    name: &str,
    units: &str,
) -> String {
    let parameter_id = create_parameter(app, admin, code, name, units).await;
    assign_site_parameter_minimal(app, admin, site_id, &parameter_id).await;
    parameter_id
}
