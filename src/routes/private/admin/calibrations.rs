use axum::{
    Json,
    extract::{Path, State},
};
use sea_orm::EntityTrait;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::AppResult;

/// The job a recalculation enqueued.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct RecalculateResponse {
    pub job_id: Uuid,
}

/// Reprocess readings for the sensor owning a specific calibration.
/// Enqueues a tracked worker job and returns immediately.
#[utoipa::path(
    post,
    path = "/api/actions/sensor_calibrations/{id}/recalculate",
    params(("id" = Uuid, Path, description = "Calibration UUID")),
    responses(
        (status = 200, description = "Reprocessing job spawned", body = RecalculateResponse),
        (status = 404, description = "Calibration not found"),
    ),
    tag = "actions"
)]
pub async fn recalculate_calibration(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<RecalculateResponse>> {
    let row = crate::routes::private::sensor_calibrations::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(|e| crate::error::AppError::Internal(e.to_string()))?;

    let Some(row) = row else {
        return Err(crate::error::AppError::NotFound(format!(
            "sensor_calibration {id} not found"
        )));
    };
    let sensor_id = row.sensor_id;

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

    Ok(Json(RecalculateResponse { job_id }))
}
