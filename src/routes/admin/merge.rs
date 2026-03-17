use axum::{Json, extract::State};

use crate::common::AppState;
use crate::error::AppResult;
use crate::services::merge::{
    MergeSiteParametersRequest, MergeSiteParametersResponse, merge_site_parameters,
};

pub async fn merge_site_parameters_handler(
    State(state): State<AppState>,
    Json(payload): Json<MergeSiteParametersRequest>,
) -> AppResult<Json<MergeSiteParametersResponse>> {
    tracing::info!(
        source = %payload.source_site_parameter_id,
        target = %payload.target_site_parameter_id,
        "Merging site_parameters"
    );

    let result = merge_site_parameters(&state.db, &payload).await?;

    tracing::info!(
        merged_readings = result.merged_readings,
        merged_status_events = result.merged_status_events,
        source_mappings_updated = result.source_mappings_updated,
        "Site parameter merge complete"
    );

    Ok(Json(result))
}
