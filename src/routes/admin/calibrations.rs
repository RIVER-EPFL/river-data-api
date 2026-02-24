use axum::{extract::{Path, State}, Json};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::AppResult;
use crate::services::calibration::recalculate_for_calibration;

pub async fn recalculate_calibration(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    let rows = recalculate_for_calibration(&state.db, id)
        .await
        .map_err(|e| crate::error::AppError::Internal(e.to_string()))?;
    Ok(Json(serde_json::json!({ "rows_updated": rows })))
}
