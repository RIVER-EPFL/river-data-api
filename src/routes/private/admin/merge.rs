use axum::{Json, extract::State};

use super::merge_services::{
    MergeParametersRequest, MergeParametersResponse, MergeSiteParametersRequest,
    MergeSiteParametersResponse,
};
use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::common::scope::{Unowned, project_of_site_parameter, require_target_in_scope};
use crate::error::AppResult;

/// Merge two `site_parameters`, absorb `source` into `target`. Moves every slot-keyed table's rows
/// (readings, status events, samples, annotations) and the streams feeding the slot, then deletes
/// the source row. All or nothing: the whole move is one transaction. Requires `write_metadata`.
///
/// Refused with 409 when source and target both hold a grab sample at the same instant: merging two
/// separately collected groups would rewrite the survivor's stored mean, sd and n.
#[utoipa::path(
    post,
    path = "/actions/merge_site_parameters",
    request_body = MergeSiteParametersRequest,
    responses(
        (status = 200, description = "Counts of moved rows and source deletion status", body = MergeSiteParametersResponse),
        (status = 403, description = "Either slot is outside the caller's projects"),
        (status = 404, description = "Source or target not found"),
        (status = 409, description = "Source and target hold a sample at the same instant"),
    ),
    tag = "actions"
)]
pub async fn merge_site_parameters_handler(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<MergeSiteParametersRequest>,
) -> AppResult<Json<serde_json::Value>> {
    // Both slots must be in scope: absorbing one slot into another is a write to both sides, so a
    // merge spanning two projects is a cross-project write even when one side is granted. Refuse
    // before enqueueing, otherwise a refused request leaves a job that performs the merge anyway.
    for site_parameter_id in [
        payload.source_site_parameter_id,
        payload.target_site_parameter_id,
    ] {
        let row = project_of_site_parameter(&state.db, site_parameter_id).await?;
        require_target_in_scope(&scope, &row, Unowned::Deny, "site parameter")?;
    }

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
    Ok(Json(
        serde_json::json!({ "job_id": job_id, "status": "queued" }),
    ))
}

/// Merge two global parameters in the catalog, absorb `source` into `target`. Re-points
/// every `site_parameter`, reading, status event, and stream from source to target. Use
/// when two catalog entries describe the same physical parameter. The merge hard-deletes the source
/// catalog row, so it holds the same Administrator gate as `DELETE /parameters/{id}`; an API token
/// carrying `write_metadata` is admitted for automation, unless it is project-scoped.
#[utoipa::path(
    post,
    path = "/actions/merge_parameters",
    request_body = MergeParametersRequest,
    responses(
        (status = 200, description = "Counts of moved rows", body = MergeParametersResponse),
        (status = 404, description = "Source or target parameter not found"),
        (status = 409, description = "Source and target hold a sample at the same instant"),
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
    Ok(Json(
        serde_json::json!({ "job_id": job_id, "status": "queued" }),
    ))
}
