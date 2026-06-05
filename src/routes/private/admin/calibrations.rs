use axum::{
    Json,
    extract::{Path, State},
};
use sea_orm::{ConnectionTrait, Statement};
use uuid::Uuid;

use crate::common::{AppState, global_event_sender};
use crate::error::AppResult;
use crate::routes::private::sensor_calibrations::services::spawn_reprocessing_job;

/// Reprocess readings for the sensor owning a specific calibration.
/// Spawns a tracked background job and returns immediately.
#[utoipa::path(
    post,
    path = "/actions/sensor_calibrations/{id}/recalculate",
    params(("id" = Uuid, Path, description = "Calibration UUID")),
    responses(
        (status = 200, description = "Reprocessing job spawned"),
        (status = 404, description = "Calibration not found"),
    ),
    tag = "actions"
)]
pub async fn recalculate_calibration(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    let row = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sensor_id FROM sensor_calibrations WHERE id = $1",
            [id.into()],
        ))
        .await
        .map_err(|e| crate::error::AppError::Internal(e.to_string()))?;

    let Some(row) = row else {
        return Err(crate::error::AppError::NotFound(
            format!("sensor_calibration {id} not found"),
        ));
    };
    let sensor_id: Uuid = row
        .try_get("", "sensor_id")
        .map_err(|e| crate::error::AppError::Internal(e.to_string()))?;

    let events = global_event_sender()
        .ok_or_else(|| crate::error::AppError::Internal("Event sender not available".into()))?;

    let job_id = spawn_reprocessing_job(
        &state.db,
        sensor_id,
        "calibration_recalculate",
        Some(id),
        events,
    )
    .await
    .map_err(|e| crate::error::AppError::Internal(e.to_string()))?;

    Ok(Json(serde_json::json!({ "job_id": job_id })))
}
