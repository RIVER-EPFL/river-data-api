//! The catalog actions that are not generated CRUD.

use axum::Json;
use axum::extract::State;

use super::service::MergeParametersRequest;
use super::service::MergeParametersResponse;
use crate::common::AppState;
use crate::error::AppResult;

/// Merge two global parameters in the catalog, absorb `source` into `target`. Re-points
/// every `site_parameter`, reading, status event, and stream from source to target. Use
/// when two catalog entries describe the same physical parameter. The merge hard-deletes the source
/// catalog row, so it holds the same Administrator gate as `DELETE /parameters/{id}`; an API token
/// carrying `write_metadata` is admitted for automation, unless it is project-scoped.
#[utoipa::path(
    post,
    path = "/api/actions/merge_parameters",
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
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(payload): Json<MergeParametersRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let trigger_id = payload.source_parameter_id;
    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
        &state.db,
        "merge_parameters",
        None,
        Some(trigger_id),
        &serde_json::json!({
            "source_parameter_id": payload.source_parameter_id,
            "target_parameter_id": payload.target_parameter_id,
            "actor": crate::common::actor::label(&auth),
            "origin": auth.origin().as_str(),
        }),
        None,
    )
    .await?;
    Ok(Json(
        serde_json::json!({ "job_id": job_id, "status": "queued" }),
    ))
}
