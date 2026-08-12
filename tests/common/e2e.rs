//! HTTP-driven workflow helpers for end-to-end tests.
//!
//! Each helper creates an entity through its real endpoint (mirroring the payloads proven by
//! `public_workflow_e2e_test.rs`) and returns its id, so an `e2e_*` test reads as a sequence of
//! user actions. `poll_job` waits on tracked reprocessing jobs; `field_for`/`values_for` pull
//! numeric arrays out of readings/aggregate responses for 1:1 assertions.

use axum::Router;
use serde_json::json;
use std::time::{Duration, Instant};

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
                   COUNT(*) FILTER (WHERE status IN ('queued','pending','running')) AS active, \
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
