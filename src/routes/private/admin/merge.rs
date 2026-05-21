use axum::{Json, extract::State};

use crate::common::AppState;
use crate::error::AppResult;
use super::merge_services::{
    MergeParametersRequest, MergeParametersResponse, MergeSiteParametersRequest,
    MergeSiteParametersResponse, merge_parameters, merge_site_parameters,
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
        streams_updated = result.streams_updated,
        "Site parameter merge complete"
    );

    Ok(Json(result))
}

pub async fn merge_parameters_handler(
    State(state): State<AppState>,
    Json(payload): Json<MergeParametersRequest>,
) -> AppResult<Json<MergeParametersResponse>> {
    tracing::info!(
        source = %payload.source_parameter_id,
        target = %payload.target_parameter_id,
        "Merging parameters"
    );

    let result = merge_parameters(&state.db, &payload).await?;

    tracing::info!(
        sites_merged = result.sites_merged,
        sites_reassigned = result.sites_reassigned,
        readings_moved = result.readings_moved,
        streams_updated = result.streams_updated,
        "Parameter merge complete"
    );

    Ok(Json(result))
}
