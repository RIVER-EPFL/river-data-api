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
use crate::error::{AppError, AppResult};

/// One reading the calibration's `[valid_from, valid_until)` window resolves.
#[derive(Debug, Serialize, ToSchema)]
pub struct CalibrationWindowPoint {
    pub time: DateTime<Utc>,
    pub raw_value: f64,
    pub calibrated_value: Option<f64>,
    pub is_flagged: bool,
}

/// The data a calibration's time window resolves — for the interactive calibration editor.
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

/// `GET /sensor_calibrations/{id}/window` — the readings a calibration window resolves. `read_data`.
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
    Path(calibration_id): Path<Uuid>,
) -> AppResult<Json<CalibrationWindowResponse>> {
    let db = &state.db;

    let cal = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT c.sensor_id, c.slope, c.intercept, c.valid_from, c.valid_until, s.parameter_id
              FROM sensor_calibrations c
              JOIN sensors s ON s.id = c.sensor_id
              WHERE c.id = $1",
            [calibration_id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound("Calibration not found".to_string()))?;

    let sensor_id: Uuid = cal.try_get("", "sensor_id")?;
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

    // The window is [valid_from, COALESCE(valid_until, 'infinity')).
    let count_row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT COUNT(*) AS c FROM readings
              WHERE sensor_id = $1 AND replicate_index = 0
                AND time >= $2
                AND time < COALESCE($3, 'infinity'::timestamptz)",
            [sensor_id.into(), vf.clone(), vu.clone()],
        ))
        .await?;
    let point_count: i64 = count_row.and_then(|r| r.try_get("", "c").ok()).unwrap_or(0);

    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT time, raw_value, calibrated_value, COALESCE(is_flagged, false) AS is_flagged
              FROM readings
              WHERE sensor_id = $1 AND replicate_index = 0
                AND time >= $2
                AND time < COALESCE($3, 'infinity'::timestamptz)
              ORDER BY time DESC
              LIMIT $4",
            [sensor_id.into(), vf, vu, MAX_POINTS.into()],
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
