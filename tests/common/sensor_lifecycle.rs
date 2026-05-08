//! Helpers for testing the sensor → calibration → deployment → reading lifecycle.
//! Abstracts raw SQL into readable operations so test scenarios read like stories.

use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use std::time::{Duration, Instant};
use uuid::Uuid;

use super::db::exec;
use super::fixtures::*;

// ============================================================================
// Types
// ============================================================================

pub struct TestSensor {
    pub id: Uuid,
    pub identity_calibration_id: Uuid,
}

#[derive(Debug)]
pub struct ReadingRow {
    pub time: DateTime<Utc>,
    pub raw_value: f64,
    pub calibrated_value: Option<f64>,
    pub site_id: Option<Uuid>,
    pub parameter_id: Option<Uuid>,
    pub sensor_id: Option<Uuid>,
    pub calibration_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
}

// ============================================================================
// Convenience
// ============================================================================

pub fn dt(s: &str) -> DateTime<Utc> {
    s.parse()
        .unwrap_or_else(|e| panic!("invalid datetime '{s}': {e}"))
}

/// Seed project, sites, global parameters, and site_parameters.
/// No streams, readings, alarms, or sensors. Fast setup for targeted tests.
pub async fn seed_base_entities(db: &DatabaseConnection) {
    exec(
        db,
        &format!(
            "INSERT INTO projects (id, name, description, data_source) VALUES \
             ('{PROJECT_ID}', 'Test River Project', 'Reprocessing tests', 'test')"
        ),
    )
    .await;

    exec(
        db,
        &format!(
            "INSERT INTO sites (id, project_id, name, latitude, longitude, altitude_m) VALUES \
             ('{SITE1_ID}', '{PROJECT_ID}', 'Upstream Station', 51.5074, -0.1278, 15.0), \
             ('{SITE2_ID}', '{PROJECT_ID}', 'Downstream Station', 51.4900, -0.1100, 8.0)"
        ),
    )
    .await;

    exec(
        db,
        &format!(
            "INSERT INTO parameters (id, name, display_name, default_units, category, data_type) VALUES \
             ('{GLOBAL_PARAM_TEMP_ID}', 'DO_Temperature', 'Water Temperature', '°C', 'measurement', 'numeric'), \
             ('{GLOBAL_PARAM_DO_ID}', 'Dissolved_O2', 'Dissolved Oxygen', 'µM', 'measurement', 'numeric'), \
             ('{GLOBAL_PARAM_COND_ID}', 'Conductivity', 'Conductivity', 'µS/cm', 'measurement', 'numeric'), \
             ('{GLOBAL_PARAM_TURB_ID}', 'Turbidity', 'Turbidity', 'NTU', 'measurement', 'numeric'), \
             ('{GLOBAL_PARAM_DEPTH_ID}', 'Depth', 'Water Depth', 'mm', 'measurement', 'numeric')"
        ),
    )
    .await;

    let configs = param_configs();
    let values: Vec<String> = configs
        .iter()
        .map(|p| {
            format!(
                "('{id}', '{site}', '{param}', '{name}', '{st}', '{u}', '{un}', {umin}, {umax}, {dp}, 600, true)",
                id = p.site_param_id, site = p.site_id, param = p.global_param_id,
                name = p.name, st = p.sensor_type, u = p.display_units,
                un = p.units_name, umin = p.units_min, umax = p.units_max, dp = p.decimal_places,
            )
        })
        .collect();

    exec(
        db,
        &format!(
            "INSERT INTO site_parameters \
             (id, site_id, parameter_id, name, sensor_type, display_units, units_name, \
              units_min, units_max, decimal_places, sample_interval_sec, is_active) VALUES {}",
            values.join(", ")
        ),
    )
    .await;
}

// ============================================================================
// Sensor lifecycle
// ============================================================================

/// Create a sensor linked to a global parameter, with an identity calibration
/// (slope=1, intercept=0, valid_from=2000-01-01).
pub async fn create_sensor(
    db: &DatabaseConnection,
    name: &str,
    parameter_id: &str,
) -> TestSensor {
    let sensor_id = Uuid::new_v4();
    let cal_id = Uuid::new_v4();

    exec(
        db,
        &format!(
            "INSERT INTO sensors (id, name, parameter_id, is_active) \
             VALUES ('{sensor_id}', '{name}', '{parameter_id}', true)"
        ),
    )
    .await;

    exec(
        db,
        &format!(
            "INSERT INTO sensor_calibrations (id, sensor_id, slope, intercept, valid_from, notes) \
             VALUES ('{cal_id}', '{sensor_id}', 1.0, 0.0, '2000-01-01T00:00:00Z', 'identity')"
        ),
    )
    .await;

    TestSensor {
        id: sensor_id,
        identity_calibration_id: cal_id,
    }
}

/// Deploy a sensor to a site starting at `from`. Returns the deployment ID.
pub async fn deploy_sensor(
    db: &DatabaseConnection,
    sensor_id: Uuid,
    site_id: &str,
    from: DateTime<Utc>,
) -> Uuid {
    let id = Uuid::new_v4();
    exec(
        db,
        &format!(
            "INSERT INTO sensor_deployments (id, sensor_id, site_id, deployed_from, deployment_type) \
             VALUES ('{id}', '{sensor_id}', '{site_id}', '{}', 'permanent')",
            from.to_rfc3339()
        ),
    )
    .await;
    id
}

/// Close a deployment at the given time.
pub async fn end_deployment(db: &DatabaseConnection, deployment_id: Uuid, until: DateTime<Utc>) {
    exec(
        db,
        &format!(
            "UPDATE sensor_deployments SET deployed_until = '{}' WHERE id = '{deployment_id}'",
            until.to_rfc3339()
        ),
    )
    .await;
}

/// Add a calibration to a sensor. Returns the calibration ID.
pub async fn add_calibration(
    db: &DatabaseConnection,
    sensor_id: Uuid,
    slope: f64,
    intercept: f64,
    valid_from: DateTime<Utc>,
) -> Uuid {
    let id = Uuid::new_v4();
    exec(
        db,
        &format!(
            "INSERT INTO sensor_calibrations (id, sensor_id, slope, intercept, valid_from) \
             VALUES ('{id}', '{sensor_id}', {slope}, {intercept}, '{}')",
            valid_from.to_rfc3339()
        ),
    )
    .await;
    id
}

/// Delete a calibration.
pub async fn delete_calibration(db: &DatabaseConnection, calibration_id: Uuid) {
    exec(
        db,
        &format!("DELETE FROM sensor_calibrations WHERE id = '{calibration_id}'"),
    )
    .await;
}

/// Create a data stream paired to a site_parameter. Returns the stream ID.
pub async fn create_paired_stream(
    db: &DatabaseConnection,
    source_key: &str,
    site_parameter_id: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    exec(
        db,
        &format!(
            "INSERT INTO data_streams \
             (id, source_system, source_key, source_name, site_parameter_id, paired_at, is_active) \
             VALUES ('{id}', 'test', '{source_key}', 'Test {source_key}', \
             '{site_parameter_id}', NOW(), true)"
        ),
    )
    .await;
    id
}

/// Insert readings with full sensor context.
/// `slope` and `intercept` are used to compute `calibrated_value = slope * raw + intercept`
/// at insert time, mimicking what the ingestion path does.
pub async fn insert_readings(
    db: &DatabaseConnection,
    stream_id: Uuid,
    site_id: &str,
    parameter_id: &str,
    sensor_id: Uuid,
    calibration_id: Uuid,
    deployment_id: Uuid,
    slope: f64,
    intercept: f64,
    readings: &[(DateTime<Utc>, f64)],
) {
    for (time, raw) in readings {
        let cal = slope * raw + intercept;
        exec(
            db,
            &format!(
                "INSERT INTO readings \
                 (stream_id, site_id, parameter_id, time, raw_value, calibrated_value, \
                  sensor_id, calibration_id, deployment_id, replicate_index) \
                 VALUES ('{stream_id}', '{site_id}', '{parameter_id}', '{}', {raw}, {cal}, \
                 '{sensor_id}', '{calibration_id}', '{deployment_id}', 0) \
                 ON CONFLICT DO NOTHING",
                time.to_rfc3339()
            ),
        )
        .await;
    }
}

/// Query all readings for a stream, ordered by time.
pub async fn get_readings(db: &DatabaseConnection, stream_id: Uuid) -> Vec<ReadingRow> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT time, raw_value, calibrated_value, site_id, parameter_id, \
                    sensor_id, calibration_id, deployment_id \
             FROM readings WHERE stream_id = $1 ORDER BY time",
            [stream_id.into()],
        ))
        .await
        .expect("query readings failed");

    rows.iter()
        .map(|r| {
            let time_tz: chrono::DateTime<chrono::FixedOffset> =
                r.try_get("", "time").unwrap();
            ReadingRow {
                time: time_tz.with_timezone(&Utc),
                raw_value: r.try_get("", "raw_value").unwrap(),
                calibrated_value: r.try_get("", "calibrated_value").ok(),
                site_id: r.try_get("", "site_id").ok(),
                parameter_id: r.try_get("", "parameter_id").ok(),
                sensor_id: r.try_get("", "sensor_id").ok(),
                calibration_id: r.try_get("", "calibration_id").ok(),
                deployment_id: r.try_get("", "deployment_id").ok(),
            }
        })
        .collect()
}

/// Wait for all pending reprocessing jobs for a sensor to complete.
/// Returns true if all completed successfully, false on failure or timeout.
pub async fn wait_for_reprocessing(
    db: &DatabaseConnection,
    sensor_id: Uuid,
    timeout: Duration,
) -> bool {
    let start = Instant::now();
    loop {
        let row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT COUNT(*) AS cnt FROM reprocessing_jobs \
                 WHERE sensor_id = $1 AND status IN ('pending', 'running')",
                [sensor_id.into()],
            ))
            .await
            .expect("query reprocessing_jobs failed");

        let pending: i64 = row.unwrap().try_get("", "cnt").unwrap();
        if pending == 0 {
            let fail_row = db
                .query_one(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT COUNT(*) AS cnt FROM reprocessing_jobs \
                     WHERE sensor_id = $1 AND status = 'failed'",
                    [sensor_id.into()],
                ))
                .await
                .expect("query reprocessing_jobs failed");
            let failed: i64 = fail_row.unwrap().try_get("", "cnt").unwrap();
            return failed == 0;
        }

        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
}

/// Query all readings for a sensor across all streams, ordered by time.
pub async fn get_readings_for_sensor(
    db: &DatabaseConnection,
    sensor_id: Uuid,
) -> Vec<ReadingRow> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT time, raw_value, calibrated_value, site_id, parameter_id, \
                    sensor_id, calibration_id, deployment_id \
             FROM readings WHERE sensor_id = $1 ORDER BY time",
            [sensor_id.into()],
        ))
        .await
        .expect("query readings for sensor failed");

    rows.iter()
        .map(|r| {
            let time_tz: chrono::DateTime<chrono::FixedOffset> =
                r.try_get("", "time").unwrap();
            ReadingRow {
                time: time_tz.with_timezone(&Utc),
                raw_value: r.try_get("", "raw_value").unwrap(),
                calibrated_value: r.try_get("", "calibrated_value").ok(),
                site_id: r.try_get("", "site_id").ok(),
                parameter_id: r.try_get("", "parameter_id").ok(),
                sensor_id: r.try_get("", "sensor_id").ok(),
                calibration_id: r.try_get("", "calibration_id").ok(),
                deployment_id: r.try_get("", "deployment_id").ok(),
            }
        })
        .collect()
}
