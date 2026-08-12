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
pub async fn refresh_hourly(db: &sea_orm::DatabaseConnection, since: chrono::DateTime<chrono::Utc>) {
    use sea_orm::{ConnectionTrait, Statement};
    db.execute(Statement::from_string(
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
/// The aggregate is grouped by `(bucket, site_id, parameter_id, sensor_id)`, so a slot served by
/// more than one sensor has one row per sensor. This collapses the sensor dimension the same way
/// `sites/aggregates.rs` does, `SUM(sum_value) / SUM(count)`, rather than averaging the per-sensor
/// averages, which would weight a sparse sensor equally with a dense one.
pub async fn hourly_bucket(
    db: &sea_orm::DatabaseConnection,
    site_id: &str,
    parameter_id: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> Option<(f64, i64)> {
    use sea_orm::{ConnectionTrait, Statement};
    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT SUM(sum_value) AS total, SUM(count) AS n FROM readings_hourly \
                 WHERE site_id = '{site_id}' AND parameter_id = '{parameter_id}' \
                   AND bucket = time_bucket('1 hour', '{}'::timestamptz)",
                at.to_rfc3339()
            ),
        ))
        .await
        .expect("query readings_hourly")?;

    let total: Option<f64> = row.try_get("", "total").ok().flatten();
    let n: Option<i64> = row.try_get("", "n").ok().flatten();
    match (total, n) {
        (Some(t), Some(c)) if c > 0 => Some((t / c as f64, c)),
        _ => None,
    }
}

/// Percent-encode a CrudCrate `filter` value for use in a query string.
pub fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The identity calibration auto-created alongside a sensor, if one exists.
///
/// Returned as an Option rather than unwrapped so a caller can assert on its presence: the
/// auto-creation is behaviour worth pinning, not an assumption to build on silently.
pub async fn identity_calibration_id(app: &Router, token: &str, sensor_id: &str) -> Option<String> {
    let filter = percent_encode(&format!(r#"{{"sensor_id":"{sensor_id}"}}"#));
    let (status, body) =
        super::get_json_with_token(app, &format!("/api/sensor_calibrations?filter={filter}"), token).await;
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

/// Poll a reprocessing job until completed/failed or the deadline elapses; returns the final status.
pub async fn poll_job(app: &Router, token: &str, job_id: &str, max_secs: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(max_secs);
    loop {
        let (_s, job) =
            super::get_json_with_token(app, &format!("/api/reprocessing_jobs/{job_id}"), token).await;
        let status = job["status"].as_str().unwrap_or("").to_string();
        if status == "completed" || status == "failed" || Instant::now() >= deadline {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Wait for all reprocessing jobs of a given `trigger_type` to reach a terminal state. Returns true
/// if at least one job ran and none failed; false on failure or timeout. For background jobs whose
/// id isn't returned by the triggering request (e.g. `derived_assignment`, which has a NULL sensor_id).
pub async fn wait_for_jobs_by_trigger(db: &sea_orm::DatabaseConnection, trigger_type: &str, timeout_secs: u64) -> bool {
    use sea_orm::{ConnectionTrait, Statement};
    let start = Instant::now();
    loop {
        let row = db
            .query_one(Statement::from_sql_and_values(
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
        if start.elapsed().as_secs() > timeout_secs {
            return false;
        }
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
        .find(|p| p["code"] == key || p["name"] == key || p["display_name"] == key || p["parameter_id"] == key)
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
    assert!((200..300).contains(&status), "create {path} ({status}): {json}");
    id_of(&json)
}

pub async fn create_project(app: &Router, token: &str, name: &str, code: &str, public: bool) -> String {
    create(
        app,
        token,
        "/api/projects",
        json!({ "name": name, "description": "e2e", "is_public": public, "public_code": code }),
    )
    .await
}

pub async fn create_site(app: &Router, token: &str, project_id: &str, name: &str, code: &str) -> String {
    create(
        app,
        token,
        "/api/sites",
        json!({ "name": name, "project_id": project_id, "latitude": 46.0, "longitude": 7.0, "public_code": code }),
    )
    .await
}

pub async fn create_parameter(app: &Router, token: &str, code: &str, name: &str, units: &str) -> String {
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
pub async fn assign_site_parameter_minimal(app: &Router, token: &str, site_id: &str, parameter_id: &str) -> String {
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

/// Mark a site_parameter public. `is_public` is excluded from create and the CrudCrate update route
/// is PUT-only, so tests set it with a direct UPDATE (matching `public_workflow_e2e_test`).
pub async fn set_site_parameter_public(db: &sea_orm::DatabaseConnection, sp_id: &str) {
    use sea_orm::{ConnectionTrait, Statement};
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("UPDATE site_parameters SET is_public = true WHERE id = '{sp_id}'"),
    ))
    .await
    .expect("mark site_parameter public");
}
