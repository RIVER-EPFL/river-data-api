use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
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
    #[schema(required)]
    pub sensor_serial: Option<String>,
    #[schema(required)]
    pub sensor_name: Option<String>,
    pub site_id: Uuid,
    #[schema(required)]
    pub site_name: Option<String>,
    pub parameter_id: Uuid,
    pub from: DateTime<Utc>,
    #[schema(required)]
    pub until: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CalibrationMarker {
    pub calibration_id: Uuid,
    pub sensor_id: Uuid,
    pub slope: f64,
    pub intercept: f64,
    pub valid_from: DateTime<Utc>,
    #[schema(required)]
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

/// `GET /sites/{site_id}/sensor_identity`, deployment bands + calibration markers per parameter.
#[utoipa::path(
    get,
    path = "/api/sites/{site_id}/sensor_identity",
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
        band_sql.push_str(" AND d.parameter_id = ANY($4)");
        values.push(pids.clone().into());
    }
    band_sql.push_str(" ORDER BY d.parameter_id, d.deployed_from");

    let band_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &band_sql,
            values.clone(),
        ))
        .await?;

    let mut bands: HashMap<Uuid, Vec<IdentityBand>> = HashMap::new();
    for row in &band_rows {
        let row = BandRow::from_query_result(row, "")?;
        bands
            .entry(row.parameter_id)
            .or_default()
            .push(IdentityBand {
                deployment_id: row.deployment_id,
                sensor_id: row.sensor_id,
                sensor_serial: row.sensor_serial,
                sensor_name: row.sensor_name,
                site_id: row.site_id,
                site_name: Some(site.name.clone()),
                parameter_id: row.parameter_id,
                from: row.deployed_from.with_timezone(&Utc),
                until: row.deployed_until.map(|u| u.with_timezone(&Utc)),
            });
    }

    // Calibration markers: calibrations (overlapping the window) of the sensors deployed at this
    // site over the window, grouped by the sensor's parameter.
    // Markers use the calibration's own parameter (the sensor no longer carries one), so a
    // calibration whose parameter is not resolved yet has no series to sit on and is not a
    // plottable marker. Lab curves cannot appear here at all: they live in `standard_curves`,
    // which has no window to plot.
    let mut cal_sql = String::from(
        r"SELECT c.id AS calibration_id, c.sensor_id, c.parameter_id,
                 c.slope, c.intercept, c.valid_from, c.valid_until
          FROM sensor_calibrations c
          WHERE c.sensor_id IN (
              SELECT DISTINCT d.sensor_id FROM sensor_deployments d
              WHERE d.site_id = $1
                AND d.deployed_from < $2
                AND COALESCE(d.deployed_until, 'infinity'::timestamptz) > $3",
    );
    if let Some(ref pids) = param_filter
        && !pids.is_empty()
    {
        cal_sql.push_str(" AND d.parameter_id = ANY($4)");
    }
    cal_sql.push_str(
        r" )
            AND c.parameter_id IS NOT NULL
            AND c.valid_from < $2
            AND COALESCE(c.valid_until, 'infinity'::timestamptz) > $3
          ORDER BY c.parameter_id, c.valid_from",
    );

    let cal_rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &cal_sql,
            values,
        ))
        .await?;

    let mut calibrations: HashMap<Uuid, Vec<CalibrationMarker>> = HashMap::new();
    for row in &cal_rows {
        let row = MarkerRow::from_query_result(row, "")?;
        calibrations
            .entry(row.parameter_id)
            .or_default()
            .push(CalibrationMarker {
                calibration_id: row.calibration_id,
                sensor_id: row.sensor_id,
                slope: row.slope,
                intercept: row.intercept,
                valid_from: row.valid_from.with_timezone(&Utc),
                valid_until: row.valid_until.map(|u| u.with_timezone(&Utc)),
            });
    }

    Ok(Json(SensorIdentityResponse {
        site_id: site.id,
        bands,
        calibrations,
    }))
}

/// The two window queries this view makes, as their SELECTs return them.
#[derive(FromQueryResult)]
struct BandRow {
    parameter_id: Uuid,
    deployment_id: Uuid,
    sensor_id: Uuid,
    sensor_serial: Option<String>,
    sensor_name: Option<String>,
    site_id: Uuid,
    deployed_from: DateTime<chrono::FixedOffset>,
    deployed_until: Option<DateTime<chrono::FixedOffset>>,
}

#[derive(FromQueryResult)]
struct MarkerRow {
    parameter_id: Uuid,
    calibration_id: Uuid,
    sensor_id: Uuid,
    slope: f64,
    intercept: f64,
    valid_from: DateTime<chrono::FixedOffset>,
    valid_until: Option<DateTime<chrono::FixedOffset>>,
}
