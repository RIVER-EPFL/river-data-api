use axum::{
    Json,
    extract::{Path, State},
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, Statement};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::{ProjectScope, sensor_in_scope};
use crate::error::{AppError, AppResult};

/// One reading the calibration's `[valid_from, valid_until)` window resolves.
#[derive(Debug, Serialize, ToSchema)]
pub struct CalibrationWindowPoint {
    pub time: DateTime<Utc>,
    pub raw_value: f64,
    pub calibrated_value: Option<f64>,
    pub is_flagged: bool,
}

/// The data a calibration's time window resolves, for the interactive calibration editor.
/// `points` is capped (most recent within the window) while `point_count` is the true total.
#[derive(Debug, Serialize, ToSchema)]
pub struct CalibrationWindowResponse {
    pub calibration_id: Uuid,
    pub sensor_id: Uuid,
    pub parameter_id: Option<Uuid>,
    pub slope: f64,
    pub intercept: f64,
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
    pub point_count: i64,
    pub points: Vec<CalibrationWindowPoint>,
}

const MAX_POINTS: i64 = 2000;

/// `GET /sensor_calibrations/{id}/window`, the readings a calibration window resolves. `read_data`.
#[utoipa::path(
    get,
    path = "/sensor_calibrations/{id}/window",
    params(("id" = Uuid, Path, description = "Calibration UUID")),
    responses(
        (status = 200, description = "Calibration window + resolved points", body = CalibrationWindowResponse),
        (status = 404, description = "Calibration not found"),
    ),
    tag = "sensors"
)]
pub async fn get_calibration_window(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(calibration_id): Path<Uuid>,
) -> AppResult<Json<CalibrationWindowResponse>> {
    let db = &state.db;

    let cal = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT c.sensor_id, c.slope, c.intercept, c.valid_from, c.valid_until, c.parameter_id
              FROM sensor_calibrations c
              WHERE c.id = $1",
            [calibration_id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound("Calibration not found".to_string()))?;

    let sensor_id: Uuid = cal.try_get("", "sensor_id")?;

    // A project-scoped key may only inspect a calibration whose sensor is deployed within its
    // project, and only sees the window's in-project readings.
    if !sensor_in_scope(db, &scope, sensor_id).await? {
        return Err(AppError::NotFound("Calibration not found".to_string()));
    }
    // Appends `AND site_id IN (<scope project's sites>)` (binding the project id) when scoped.
    let scope_clause = |values: &mut Vec<sea_orm::Value>| -> String {
        match scope.sql_project_array() {
            Some(projects) => {
                values.push(projects);
                format!(
                    " AND site_id IN (SELECT id FROM sites WHERE project_id = ANY(${}))",
                    values.len()
                )
            }
            None => String::new(),
        }
    };
    let parameter_id: Option<Uuid> = cal.try_get("", "parameter_id").ok();
    let slope: f64 = cal.try_get("", "slope")?;
    let intercept: f64 = cal.try_get("", "intercept")?;
    let valid_from: DateTime<chrono::FixedOffset> = cal.try_get("", "valid_from")?;
    let valid_until: Option<DateTime<chrono::FixedOffset>> = cal.try_get("", "valid_until").ok();

    let vf: sea_orm::Value = valid_from.into();
    let vu: sea_orm::Value = match valid_until {
        Some(u) => u.into(),
        None => sea_orm::Value::ChronoDateTimeWithTimeZone(None),
    };

    // The window is [valid_from, COALESCE(valid_until, 'infinity')). The count is per instant:
    // continuous and derived rows live at replicate_index 0, so their count is a plain COUNT(*)
    // with no sort; a spot instant is the replicate group `(stream_id, time)`, and the composite
    // DISTINCT is confined to that small subset.
    let mut count_vals: Vec<sea_orm::Value> = vec![sensor_id.into(), vf.clone(), vu.clone()];
    let count_scope = scope_clause(&mut count_vals);
    let count_row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &format!(
                r"SELECT (SELECT COUNT(*) FROM readings
                          WHERE sensor_id = $1
                            AND time >= $2
                            AND time < COALESCE($3, 'infinity'::timestamptz)
                            AND replicate_index = 0
                            AND measurement_type IS DISTINCT FROM 'spot'{count_scope})
                       + (SELECT COUNT(DISTINCT (stream_id, time)) FROM readings
                          WHERE sensor_id = $1
                            AND time >= $2
                            AND time < COALESCE($3, 'infinity'::timestamptz)
                            AND measurement_type = 'spot'{count_scope}) AS c"
            ),
            count_vals,
        ))
        .await?;
    let point_count: i64 = count_row.and_then(|r| r.try_get("", "c").ok()).unwrap_or(0);

    // Each arm carries its own LIMIT so the continuous arm keeps the index-backed early stop; the
    // outer sort then orders at most twice the cap. The spot arm collapses a replicate group to
    // its lowest unflagged replicate (a flagged-only group surfaces its flagged row: the editor
    // shows flagged points).
    let mut point_vals: Vec<sea_orm::Value> = vec![sensor_id.into(), vf, vu];
    let point_scope = scope_clause(&mut point_vals);
    point_vals.push(MAX_POINTS.into());
    let limit_idx = point_vals.len();
    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &format!(
                r"SELECT time, raw_value, calibrated_value, is_flagged FROM (
                      (SELECT time, raw_value, calibrated_value,
                              COALESCE(is_flagged, false) AS is_flagged
                       FROM readings
                       WHERE sensor_id = $1
                         AND time >= $2
                         AND time < COALESCE($3, 'infinity'::timestamptz)
                         AND replicate_index = 0
                         AND measurement_type IS DISTINCT FROM 'spot'{point_scope}
                       ORDER BY time DESC
                       LIMIT ${limit_idx})
                      UNION ALL
                      (SELECT time, raw_value, calibrated_value, is_flagged FROM (
                          SELECT DISTINCT ON (stream_id, time)
                                 time, raw_value, calibrated_value,
                                 COALESCE(is_flagged, false) AS is_flagged
                          FROM readings
                          WHERE sensor_id = $1
                            AND time >= $2
                            AND time < COALESCE($3, 'infinity'::timestamptz)
                            AND measurement_type = 'spot'{point_scope}
                          ORDER BY stream_id, time, (is_flagged IS TRUE), replicate_index
                       ) sp
                       ORDER BY time DESC
                       LIMIT ${limit_idx})
                  ) w
                  ORDER BY time DESC
                  LIMIT ${limit_idx}"
            ),
            point_vals,
        ))
        .await?;

    let mut points = rows
        .iter()
        .map(|row| -> AppResult<CalibrationWindowPoint> {
            let t: DateTime<chrono::FixedOffset> = row.try_get("", "time")?;
            Ok(CalibrationWindowPoint {
                time: t.with_timezone(&Utc),
                raw_value: row.try_get("", "raw_value")?,
                calibrated_value: row.try_get::<f64>("", "calibrated_value").ok(),
                is_flagged: row.try_get("", "is_flagged").unwrap_or(false),
            })
        })
        .collect::<AppResult<Vec<_>>>()?;
    points.reverse(); // chronological for the scatter

    Ok(Json(CalibrationWindowResponse {
        calibration_id,
        sensor_id,
        parameter_id,
        slope,
        intercept,
        valid_from: valid_from.with_timezone(&Utc),
        valid_until: valid_until.map(|u| u.with_timezone(&Utc)),
        point_count,
        points,
    }))
}
