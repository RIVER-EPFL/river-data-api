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
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize, IntoParams)]
pub struct SensorReadingsQuery {
    /// Start time (ISO 8601). Defaults to the earliest reading.
    pub start: Option<DateTime<Utc>>,
    /// End time (ISO 8601). Defaults to now.
    pub end: Option<DateTime<Utc>>,
    /// Include the per-point raw (uncalibrated) value array. Default true.
    pub include_raw: Option<bool>,
}

/// Columnar raw + calibrated series for one sensor (the sensor's single global parameter),
/// aligned to `times`. `site_ids[i]` is the site the reading was attributed to (null when the
/// sensor was undeployed at that time).
#[derive(Debug, Serialize, ToSchema)]
pub struct SensorReadingsResponse {
    pub sensor_id: Uuid,
    pub parameter_id: Option<Uuid>,
    pub units: Option<String>,
    pub times: Vec<DateTime<Utc>>,
    pub raw: Vec<Option<f64>>,
    pub calibrated: Vec<Option<f64>>,
    pub site_ids: Vec<Option<Uuid>>,
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
        (status = 200, description = "Sensor readings", body = SensorReadingsResponse),
        (status = 404, description = "Sensor not found"),
    ),
    tag = "sensors"
)]
pub async fn get_sensor_readings(
    State(state): State<AppState>,
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

    let mut sql = String::from(
        r"SELECT time, raw_value, calibrated_value, site_id
          FROM readings
          WHERE sensor_id = $1 AND replicate_index = 0",
    );
    let mut values: Vec<sea_orm::Value> = vec![sensor_id.into()];
    if let Some(start) = query.start {
        values.push(start.into());
        sql.push_str(&format!(" AND time >= ${}", values.len()));
    }
    if let Some(end) = query.end {
        values.push(end.into());
        sql.push_str(&format!(" AND time <= ${}", values.len()));
    }
    sql.push_str(" ORDER BY time ASC");

    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?;

    let mut times = Vec::with_capacity(rows.len());
    let mut raw = Vec::with_capacity(rows.len());
    let mut calibrated = Vec::with_capacity(rows.len());
    let mut site_ids = Vec::with_capacity(rows.len());
    for row in &rows {
        let t: DateTime<chrono::FixedOffset> = row.try_get("", "time")?;
        times.push(t.with_timezone(&Utc));
        raw.push(if include_raw { row.try_get::<f64>("", "raw_value").ok() } else { None });
        calibrated.push(row.try_get::<f64>("", "calibrated_value").ok());
        site_ids.push(row.try_get::<Uuid>("", "site_id").ok());
    }

    Ok(Json(SensorReadingsResponse {
        sensor_id,
        parameter_id,
        units,
        times,
        raw,
        calibrated,
        site_ids,
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
    Path(sensor_id): Path<Uuid>,
    Query(query): Query<SensorBandsQuery>,
) -> AppResult<Json<SensorDeploymentBandsResponse>> {
    let db = &state.db;

    let mut sql = String::from(
        r"SELECT d.id AS deployment_id, d.site_id, s.name AS site_name,
                 d.deployed_from, d.deployed_until
          FROM sensor_deployments d
          JOIN sites s ON s.id = d.site_id
          WHERE d.sensor_id = $1",
    );
    let mut values: Vec<sea_orm::Value> = vec![sensor_id.into()];
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
