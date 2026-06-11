use axum::{Json, extract::State};

use crate::common::AppState;
use crate::error::AppResult;
use super::merge_services::{
    MergeParametersRequest, MergeParametersResponse, MergeSiteParametersRequest,
    MergeSiteParametersResponse, merge_parameters, merge_site_parameters,
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
    // Background the multi-table move as a tracked job; the job's `detail` carries the counts the
    // UI used to read synchronously. Alarm reconcile runs on job completion (central lifecycle).
    let trigger_id = payload.source_site_parameter_id;
    let job_id = crate::routes::private::reprocessing_jobs::lifecycle::spawn_tracked_job_ctx(
        &state.db,
        None,
        "merge_site_parameters",
        Some(trigger_id),
        state.events.clone(),
        move |ctx| {
            let payload = payload.clone();
            async move {
                match merge_site_parameters(ctx.db(), &payload).await {
                    Ok(result) => {
                        ctx.set_detail(serde_json::json!({ "counts": result })).await;
                        Ok(i64::try_from(result.merged_readings).unwrap_or(i64::MAX))
                    }
                    Err(e) => Err(sea_orm::DbErr::Custom(e.to_string())),
                }
            }
        },
    )
    .await?;
    Ok(Json(serde_json::json!({ "job_id": job_id, "status": "pending" })))
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
    let job_id = crate::routes::private::reprocessing_jobs::lifecycle::spawn_tracked_job_ctx(
        &state.db,
        None,
        "merge_parameters",
        Some(trigger_id),
        state.events.clone(),
        move |ctx| {
            let payload = payload.clone();
            async move {
                match merge_parameters(ctx.db(), &payload).await {
                    Ok(result) => {
                        ctx.set_detail(serde_json::json!({ "counts": result })).await;
                        Ok(i64::try_from(result.readings_moved).unwrap_or(i64::MAX))
                    }
                    Err(e) => Err(sea_orm::DbErr::Custom(e.to_string())),
                }
            }
        },
    )
    .await?;
    Ok(Json(serde_json::json!({ "job_id": job_id, "status": "pending" })))
}
