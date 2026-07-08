use axum::{
    Json,
    extract::{Path, State},
};
use sea_orm::{ConnectionTrait, Statement};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::AppResult;

/// Reprocess readings for the sensor owning a specific calibration.
/// Enqueues a tracked worker job and returns immediately.
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

    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        &state.db,
        "calibration_recalculate",
        Some(sensor_id),
        Some(id),
        &serde_json::json!({ "sensor_id": sensor_id }),
        None,
    )
    .await
    .map_err(|e| crate::error::AppError::Internal(e.to_string()))?
    .ok_or_else(|| crate::error::AppError::Internal("enqueue returned no id".into()))?;

    Ok(Json(serde_json::json!({ "job_id": job_id })))
}
