use axum::{Json, extract::State};

use crate::common::AppState;
use crate::error::AppResult;
use super::merge_services::{
    MergeParametersRequest, MergeParametersResponse, MergeSiteParametersRequest,
    MergeSiteParametersResponse,
};

/// Merge two `site_parameters` — absorb `source` into `target`. Moves readings, status
/// events, streams, and sensor deployments; deletes the source row. Idempotent on the
/// `(stream_id, time, replicate_index)` PK. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/actions/merge_site_parameters",
    request_body = MergeSiteParametersRequest,
    responses(
        (status = 200, description = "Counts of moved rows and source deletion status", body = MergeSiteParametersResponse),
        (status = 404, description = "Source or target not found"),
    ),
    tag = "actions"
)]
pub async fn merge_site_parameters_handler(
    State(state): State<AppState>,
    Json(payload): Json<MergeSiteParametersRequest>,
) -> AppResult<Json<serde_json::Value>> {
    // Background the multi-table move on the worker pool; the job's `detail` carries the counts the
    // UI used to read synchronously. Alarm reconcile runs on job completion (central lifecycle).
    let trigger_id = payload.source_site_parameter_id;
    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        &state.db,
        "merge_site_parameters",
        None,
        Some(trigger_id),
        &serde_json::json!({
            "source_site_parameter_id": payload.source_site_parameter_id,
            "target_site_parameter_id": payload.target_site_parameter_id,
        }),
        None,
    )
    .await?;
    Ok(Json(serde_json::json!({ "job_id": job_id, "status": "queued" })))
}

/// Merge two global parameters in the catalog — absorb `source` into `target`. Re-points
/// every `site_parameter`, reading, status event, and stream from source to target. Use
/// when two catalog entries describe the same physical parameter. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/actions/merge_parameters",
    request_body = MergeParametersRequest,
    responses(
        (status = 200, description = "Counts of moved rows", body = MergeParametersResponse),
        (status = 404, description = "Source or target parameter not found"),
    ),
    tag = "actions"
)]
pub async fn merge_parameters_handler(
    State(state): State<AppState>,
    Json(payload): Json<MergeParametersRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let trigger_id = payload.source_parameter_id;
    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        &state.db,
        "merge_parameters",
        None,
        Some(trigger_id),
        &serde_json::json!({
            "source_parameter_id": payload.source_parameter_id,
            "target_parameter_id": payload.target_parameter_id,
        }),
        None,
    )
    .await?;
    Ok(Json(serde_json::json!({ "job_id": job_id, "status": "queued" })))
}
