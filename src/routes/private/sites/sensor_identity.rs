use std::collections::HashMap;

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
use crate::common::middleware::ProjectScope;
use crate::error::{AppError, AppResult};
use crate::routes::{resolve_site, validate_time_range};

#[derive(Debug, Deserialize, IntoParams)]
pub struct SensorIdentityQuery {
    /// Start time (required, ISO 8601).
    pub start: DateTime<Utc>,
    /// End time (required, ISO 8601).
    pub end: DateTime<Utc>,
    /// Optional comma-separated global parameter UUIDs to restrict to.
    pub parameter_ids: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IdentityBand {
    pub deployment_id: Uuid,
    pub sensor_id: Uuid,
    pub sensor_serial: Option<String>,
    pub sensor_name: Option<String>,
    pub site_id: Uuid,
    pub site_name: Option<String>,
    pub parameter_id: Uuid,
    pub from: DateTime<Utc>,
    pub until: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CalibrationMarker {
    pub calibration_id: Uuid,
    pub sensor_id: Uuid,
    pub slope: f64,
    pub intercept: f64,
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
}

/// Sensor-identity bands + calibration markers for a site over a window, keyed by global
/// `parameter_id`. Drives the chart overlays. Sourced from the deployment/calibration tables so
/// it is correct mid-reprocess.
#[derive(Debug, Serialize, ToSchema)]
pub struct SensorIdentityResponse {
    pub site_id: Uuid,
    pub bands: HashMap<Uuid, Vec<IdentityBand>>,
    pub calibrations: HashMap<Uuid, Vec<CalibrationMarker>>,
}

fn parse_uuid_csv(s: &str) -> Vec<Uuid> {
    s.split(',')
        .filter_map(|p| Uuid::parse_str(p.trim()).ok())
        .collect()
}

/// `GET /sites/{site_id}/sensor_identity` — deployment bands + calibration markers per parameter.
#[utoipa::path(
    get,
    path = "/{site_id}/sensor_identity",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
        SensorIdentityQuery
    ),
    responses(
        (status = 200, description = "Identity bands + calibration markers", body = SensorIdentityResponse),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn get_site_sensor_identity(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
    Query(query): Query<SensorIdentityQuery>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<SensorIdentityResponse>> {
    let db = &state.db;
    let site = resolve_site(db, &site_id).await?;
    if !scope.allows_project_opt(site.project_id) {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }
    validate_time_range(query.start, query.end)?;

    let param_filter = query.parameter_ids.as_deref().map(parse_uuid_csv);

    // $1 = site_id, $2 = end, $3 = start, then optional parameter_ids.
    let mut band_sql = String::from(
        r"SELECT d.id AS deployment_id, d.sensor_id, s.serial_number AS sensor_serial,
                 s.name AS sensor_name, d.site_id, d.parameter_id, d.deployed_from, d.deployed_until
          FROM sensor_deployments d
          JOIN sensors s ON s.id = d.sensor_id
          WHERE d.site_id = $1
            AND d.deployed_from < $2
            AND COALESCE(d.deployed_until, 'infinity'::timestamptz) > $3",
    );
    let mut values: Vec<sea_orm::Value> =
        vec![site.id.into(), query.end.into(), query.start.into()];
    if let Some(ref pids) = param_filter
        && !pids.is_empty()
    {
        let ph: Vec<String> = pids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("${}", i + 4))
            .collect();
        band_sql.push_str(&format!(" AND d.parameter_id IN ({})", ph.join(",")));
        values.extend(pids.iter().map(|id| (*id).into()));
    }
    band_sql.push_str(" ORDER BY d.parameter_id, d.deployed_from");

    let band_rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &band_sql,
            values.clone(),
        ))
        .await?;

    let mut bands: HashMap<Uuid, Vec<IdentityBand>> = HashMap::new();
    for row in &band_rows {
        let parameter_id: Uuid = row.try_get("", "parameter_id")?;
        let from: DateTime<chrono::FixedOffset> = row.try_get("", "deployed_from")?;
        let until: Option<DateTime<chrono::FixedOffset>> = row.try_get("", "deployed_until").ok();
        bands.entry(parameter_id).or_default().push(IdentityBand {
            deployment_id: row.try_get("", "deployment_id")?,
            sensor_id: row.try_get("", "sensor_id")?,
            sensor_serial: row.try_get("", "sensor_serial").ok(),
            sensor_name: row.try_get("", "sensor_name").ok(),
            site_id: row.try_get("", "site_id")?,
            site_name: Some(site.name.clone()),
            parameter_id,
            from: from.with_timezone(&Utc),
            until: until.map(|u| u.with_timezone(&Utc)),
        });
    }

    // Calibration markers: calibrations (overlapping the window) of the sensors deployed at this
    // site over the window, grouped by the sensor's parameter.
    let mut cal_sql = String::from(
        r"SELECT c.id AS calibration_id, c.sensor_id, sn.parameter_id,
                 c.slope, c.intercept, c.valid_from, c.valid_until
          FROM sensor_calibrations c
          JOIN sensors sn ON sn.id = c.sensor_id
          WHERE c.sensor_id IN (
              SELECT DISTINCT d.sensor_id FROM sensor_deployments d
              WHERE d.site_id = $1
                AND d.deployed_from < $2
                AND COALESCE(d.deployed_until, 'infinity'::timestamptz) > $3",
    );
    if let Some(ref pids) = param_filter
        && !pids.is_empty()
    {
        let ph: Vec<String> = pids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("${}", i + 4))
            .collect();
        cal_sql.push_str(&format!(" AND d.parameter_id IN ({})", ph.join(",")));
    }
    cal_sql.push_str(
        r" )
            AND c.valid_from < $2
            AND COALESCE(c.valid_until, 'infinity'::timestamptz) > $3
          ORDER BY sn.parameter_id, c.valid_from",
    );

    let cal_rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &cal_sql,
            values,
        ))
        .await?;

    let mut calibrations: HashMap<Uuid, Vec<CalibrationMarker>> = HashMap::new();
    for row in &cal_rows {
        let parameter_id: Uuid = row.try_get("", "parameter_id")?;
        let valid_from: DateTime<chrono::FixedOffset> = row.try_get("", "valid_from")?;
        let valid_until: Option<DateTime<chrono::FixedOffset>> = row.try_get("", "valid_until").ok();
        calibrations
            .entry(parameter_id)
            .or_default()
            .push(CalibrationMarker {
                calibration_id: row.try_get("", "calibration_id")?,
                sensor_id: row.try_get("", "sensor_id")?,
                slope: row.try_get("", "slope")?,
                intercept: row.try_get("", "intercept")?,
                valid_from: valid_from.with_timezone(&Utc),
                valid_until: valid_until.map(|u| u.with_timezone(&Utc)),
            });
    }

    Ok(Json(SensorIdentityResponse {
        site_id: site.id,
        bands,
        calibrations,
    }))
}
