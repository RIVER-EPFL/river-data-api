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

/// Mark a site_parameter public. `is_public` is excluded from create and the CrudCrate update route
/// is PUT-only, so tests set it with a direct UPDATE (matching `public_workflow_e2e_test`).
pub async fn set_site_parameter_public(db: &sea_orm::DatabaseConnection, sp_id: &str) {
    use sea_orm::{ConnectionTrait, Statement};
    db.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("UPDATE site_parameters SET is_public = true WHERE id = '{sp_id}'"),
    ))
    .await
    .expect("mark site_parameter public");
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

/// The open event-audit findings at a site, read from the review queue the dashboard reads.
pub async fn pending_event_findings(app: &Router, token: &str, site_id: &str) -> Vec<serde_json::Value> {
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
