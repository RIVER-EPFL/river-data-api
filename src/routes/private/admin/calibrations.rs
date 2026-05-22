use axum::{
    Json,
    extract::{Path, State},
};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::AppResult;
use crate::routes::private::sensor_calibrations::services::recalculate_for_calibration;

/// Recalculate `calibrated_value` for all readings tagged with a specific calibration ID.
/// Run this after editing a calibration's slope/intercept. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/actions/sensor_calibrations/{id}/recalculate",
    params(("id" = Uuid, Path, description = "Calibration UUID")),
    responses(
        (status = 200, description = "Recalculation complete, rows_updated count returned"),
        (status = 404, description = "Calibration not found"),
    ),
    tag = "actions"
)]
pub async fn recalculate_calibration(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    let rows = recalculate_for_calibration(&state.db, id)
        .await
        .map_err(|e| crate::error::AppError::Internal(e.to_string()))?;
    Ok(Json(serde_json::json!({ "rows_updated": rows })))
}
