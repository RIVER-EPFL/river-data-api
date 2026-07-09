use axum::{
    Json,
    extract::{Path, Query, State},
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, Statement};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::{ProjectScope, sensor_in_scope};
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize, IntoParams)]
pub struct SensorReadingsQuery {
    /// Start time (ISO 8601). Defaults to the earliest reading.
    pub start: Option<DateTime<Utc>>,
    /// End time (ISO 8601). Defaults to now.
    pub end: Option<DateTime<Utc>>,
    /// Include the per-point raw (uncalibrated) value array. Default true.
    pub include_raw: Option<bool>,
    /// Downsampling resolution: `raw` (default, per-point), or `hourly`/`daily`/`weekly`/`monthly`
    /// time-bucketed averages with min/max envelopes. Mirrors the site plot's resolution selector.
    pub resolution: Option<String>,
}

/// Map a resolution keyword to a `time_bucket` interval. `None`/`raw` → no bucketing.
fn resolution_interval(res: &str) -> Option<&'static str> {
    match res {
        "hourly" => Some("1 hour"),
        "daily" => Some("1 day"),
        "weekly" => Some("7 days"),
        "monthly" => Some("1 month"),
        _ => None,
    }
}

/// Columnar raw + calibrated series for one sensor (the sensor's single global parameter),
/// aligned to `times`. `site_ids[i]` is the site the reading was attributed to (null when the
/// sensor was undeployed at that time). In an aggregated `resolution`, `raw`/`calibrated` are the
/// per-bucket averages and the `*_min`/`*_max` envelopes are populated (empty in `raw` mode).
#[derive(Debug, Serialize, ToSchema)]
pub struct SensorReadingsResponse {
    pub sensor_id: Uuid,
    pub parameter_id: Option<Uuid>,
    pub units: Option<String>,
    /// Resolution actually applied (`raw`, `hourly`, `daily`, `weekly`, `monthly`).
    pub resolution: String,
    pub times: Vec<DateTime<Utc>>,
    pub raw: Vec<Option<f64>>,
    pub calibrated: Vec<Option<f64>>,
    pub raw_min: Vec<Option<f64>>,
    pub raw_max: Vec<Option<f64>>,
    pub calibrated_min: Vec<Option<f64>>,
    pub calibrated_max: Vec<Option<f64>>,
    pub site_ids: Vec<Option<Uuid>>,
    /// Earliest/latest reading time **attributed to this sensor** (full extent, independent of the
    /// query window).
    pub data_start: Option<DateTime<Utc>>,
    pub data_end: Option<DateTime<Utc>>,
    /// Earliest reading at the sensor's current (open) deployment slot — same site + parameter,
    /// regardless of `sensor_id`. This is the true backdate target: history before `data_start`
    /// that is not yet attributed to the sensor but would be claimed by backdating `deployed_from`.
    /// Null when the sensor has no open deployment.
    pub slot_data_start: Option<DateTime<Utc>>,
}

/// Per-sensor time series (raw + calibrated), for the sensor detail plot. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/sensors/{id}/readings",
    params(
        ("id" = Uuid, Path, description = "Sensor UUID"),
        SensorReadingsQuery
    ),
    responses(
        (status = 200, description = "Sensor readings (per-point, or time-bucketed when resolution is set)", body = SensorReadingsResponse),
        (status = 400, description = "Invalid resolution"),
        (status = 404, description = "Sensor not found"),
    ),
    tag = "sensors"
)]
pub async fn get_sensor_readings(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(sensor_id): Path<Uuid>,
    Query(query): Query<SensorReadingsQuery>,
) -> AppResult<Json<SensorReadingsResponse>> {
    let db = &state.db;
    let include_raw = query.include_raw.unwrap_or(true);

    let meta = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT s.parameter_id, p.default_units
              FROM sensors s LEFT JOIN parameters p ON p.id = s.parameter_id
              WHERE s.id = $1",
            [sensor_id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound("Sensor not found".to_string()))?;
    let parameter_id: Option<Uuid> = meta.try_get("", "parameter_id").ok();
    let units: Option<String> = meta.try_get("", "default_units").ok();

    // A project-scoped key may only read a sensor that has been deployed within its project, and
    // even then only sees the readings attributed to in-project sites. A sensor never deployed in
    // the project is reported as not-found (no cross-project existence disclosure).
    if !sensor_in_scope(db, &scope, sensor_id).await? {
        return Err(AppError::NotFound("Sensor not found".to_string()));
    }
    // Appends `AND <col> IN (<scope project's sites>)` (and binds the project id) when scoped; a
    // no-op for unscoped principals. Applied to every readings query below so the temporal extent
    // and per-point series are both confined to the project.
    let scope_filter = |values: &mut Vec<sea_orm::Value>, col: &str| -> String {
        match scope.sql_project_array() {
            Some(projects) => {
                values.push(projects);
                format!(
                    " AND {col} IN (SELECT id FROM sites WHERE project_id = ANY(${}))",
                    values.len()
                )
            }
            None => String::new(),
        }
    };

    let resolution = query.resolution.as_deref().unwrap_or("raw");
    let bucket = resolution_interval(resolution);
    if query.resolution.as_deref().is_some_and(|r| r != "raw" && bucket.is_none()) {
        return Err(AppError::BadRequest(format!(
            "Invalid resolution '{resolution}' (expected raw|hourly|daily|weekly|monthly)"
        )));
    }

    // Full reading extent for this sensor (drives the UI slider bounds, independent of the window).
    let mut extent_vals: Vec<sea_orm::Value> = vec![sensor_id.into()];
    let extent_scope = scope_filter(&mut extent_vals, "site_id");
    let extent = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &format!(
                r"SELECT MIN(time) AS data_start, MAX(time) AS data_end
                  FROM readings WHERE sensor_id = $1 AND replicate_index = 0{extent_scope}"
            ),
            extent_vals,
        ))
        .await?;
    let data_start = extent
        .as_ref()
        .and_then(|r| r.try_get::<DateTime<chrono::FixedOffset>>("", "data_start").ok())
        .map(|t| t.with_timezone(&Utc));
    let data_end = extent
        .as_ref()
        .and_then(|r| r.try_get::<DateTime<chrono::FixedOffset>>("", "data_end").ok())
        .map(|t| t.with_timezone(&Utc));

    // Earliest reading at the sensor's OPEN deployment slot (same site + parameter, any sensor_id) —
    // the true backdate target. Unattributed history (sensor_id NULL) is invisible to `data_start`
    // but lives here, and backdating `deployed_from` to it lets the slot reprocess claim it.
    let mut slot_vals: Vec<sea_orm::Value> = vec![sensor_id.into()];
    let slot_scope = scope_filter(&mut slot_vals, "r.site_id");
    let slot_data_start = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &format!(
                r"SELECT MIN(r.time) AS slot_start
                  FROM readings r
                  JOIN sensor_deployments d
                    ON d.sensor_id = $1 AND d.deployed_until IS NULL
                  WHERE r.site_id = d.site_id AND r.parameter_id = d.parameter_id
                    AND r.replicate_index = 0{slot_scope}"
            ),
            slot_vals,
        ))
        .await?
        .and_then(|r| r.try_get::<DateTime<chrono::FixedOffset>>("", "slot_start").ok())
        .map(|t| t.with_timezone(&Utc));

    // Build the query. Aggregated resolutions time-bucket the readings (avg + min/max of raw and
    // calibrated), excluding flagged points to match continuous-aggregate semantics; raw mode
    // returns per-point values including flagged.
    let mut values: Vec<sea_orm::Value> = vec![sensor_id.into()];
    let mut sql = if let Some(interval) = bucket {
        format!(
            r"SELECT time_bucket('{interval}'::interval, time) AS time,
                     avg(raw_value) AS raw_value, min(raw_value) AS raw_min, max(raw_value) AS raw_max,
                     avg(calibrated_value) AS calibrated_value, min(calibrated_value) AS cal_min, max(calibrated_value) AS cal_max,
                     last(site_id, time) AS site_id
              FROM readings
              WHERE sensor_id = $1 AND replicate_index = 0 AND is_flagged IS NOT TRUE"
        )
    } else {
        String::from(
            r"SELECT time, raw_value, calibrated_value, site_id
              FROM readings
              WHERE sensor_id = $1 AND replicate_index = 0",
        )
    };
    sql.push_str(&scope_filter(&mut values, "site_id"));
    if let Some(start) = query.start {
        values.push(start.into());
        sql.push_str(&format!(" AND time >= ${}", values.len()));
    }
    if let Some(end) = query.end {
        values.push(end.into());
        sql.push_str(&format!(" AND time <= ${}", values.len()));
    }
    if bucket.is_some() {
        sql.push_str(" GROUP BY 1 ORDER BY 1 ASC");
    } else {
        sql.push_str(" ORDER BY time ASC");
    }

    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?;

    let aggregated = bucket.is_some();
    let mut times = Vec::with_capacity(rows.len());
    let mut raw = Vec::with_capacity(rows.len());
    let mut calibrated = Vec::with_capacity(rows.len());
    let mut raw_min = Vec::with_capacity(if aggregated { rows.len() } else { 0 });
    let mut raw_max = Vec::with_capacity(if aggregated { rows.len() } else { 0 });
    let mut calibrated_min = Vec::with_capacity(if aggregated { rows.len() } else { 0 });
    let mut calibrated_max = Vec::with_capacity(if aggregated { rows.len() } else { 0 });
    let mut site_ids = Vec::with_capacity(rows.len());
    for row in &rows {
        let t: DateTime<chrono::FixedOffset> = row.try_get("", "time")?;
        times.push(t.with_timezone(&Utc));
        raw.push(if include_raw { row.try_get::<f64>("", "raw_value").ok() } else { None });
        calibrated.push(row.try_get::<f64>("", "calibrated_value").ok());
        site_ids.push(row.try_get::<Uuid>("", "site_id").ok());
        if aggregated {
            raw_min.push(row.try_get::<f64>("", "raw_min").ok());
            raw_max.push(row.try_get::<f64>("", "raw_max").ok());
            calibrated_min.push(row.try_get::<f64>("", "cal_min").ok());
            calibrated_max.push(row.try_get::<f64>("", "cal_max").ok());
        }
    }

    Ok(Json(SensorReadingsResponse {
        sensor_id,
        parameter_id,
        units,
        resolution: if aggregated { resolution.to_string() } else { "raw".to_string() },
        times,
        raw,
        calibrated,
        raw_min,
        raw_max,
        calibrated_min,
        calibrated_max,
        site_ids,
        data_start,
        data_end,
        slot_data_start,
    }))
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct SensorBandsQuery {
    /// Clip bands to this start (ISO 8601).
    pub start: Option<DateTime<Utc>>,
    /// Clip bands to this end (ISO 8601).
    pub end: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SensorDeploymentBand {
    pub deployment_id: Uuid,
    pub site_id: Uuid,
    pub site_name: Option<String>,
    pub from: DateTime<Utc>,
    pub until: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SensorDeploymentBandsResponse {
    pub sensor_id: Uuid,
    pub bands: Vec<SensorDeploymentBand>,
}

/// Deployment timeline for a sensor (site-assignment bands), sourced from the deployment table so
/// it is correct mid-reprocess. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/sensors/{id}/deployment_bands",
    params(
        ("id" = Uuid, Path, description = "Sensor UUID"),
        SensorBandsQuery
    ),
    responses(
        (status = 200, description = "Deployment bands", body = SensorDeploymentBandsResponse),
    ),
    tag = "sensors"
)]
pub async fn get_sensor_deployment_bands(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(sensor_id): Path<Uuid>,
    Query(query): Query<SensorBandsQuery>,
) -> AppResult<Json<SensorDeploymentBandsResponse>> {
    let db = &state.db;

    // A project-scoped key only sees a sensor deployed within its project, and only the bands at
    // in-project sites (a field sensor can move between projects).
    if !sensor_in_scope(db, &scope, sensor_id).await? {
        return Err(AppError::NotFound("Sensor not found".to_string()));
    }

    let mut sql = String::from(
        r"SELECT d.id AS deployment_id, d.site_id, s.name AS site_name,
                 d.deployed_from, d.deployed_until
          FROM sensor_deployments d
          JOIN sites s ON s.id = d.site_id
          WHERE d.sensor_id = $1",
    );
    let mut values: Vec<sea_orm::Value> = vec![sensor_id.into()];
    if let Some(projects) = scope.sql_project_array() {
        values.push(projects);
        sql.push_str(&format!(" AND s.project_id = ANY(${})", values.len()));
    }
    if let Some(end) = query.end {
        values.push(end.into());
        sql.push_str(&format!(" AND d.deployed_from < ${}", values.len()));
    }
    if let Some(start) = query.start {
        values.push(start.into());
        sql.push_str(&format!(
            " AND COALESCE(d.deployed_until, 'infinity'::timestamptz) > ${}",
            values.len()
        ));
    }
    sql.push_str(" ORDER BY d.deployed_from ASC");

    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?;

    let bands = rows
        .iter()
        .map(|row| -> AppResult<SensorDeploymentBand> {
            let from: DateTime<chrono::FixedOffset> = row.try_get("", "deployed_from")?;
            let until: Option<DateTime<chrono::FixedOffset>> =
                row.try_get("", "deployed_until").ok();
            Ok(SensorDeploymentBand {
                deployment_id: row.try_get("", "deployment_id")?,
                site_id: row.try_get("", "site_id")?,
                site_name: row.try_get("", "site_name").ok(),
                from: from.with_timezone(&Utc),
                until: until.map(|u| u.with_timezone(&Utc)),
            })
        })
        .collect::<AppResult<Vec<_>>>()?;

    Ok(Json(SensorDeploymentBandsResponse { sensor_id, bands }))
}
