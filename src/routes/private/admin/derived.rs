use axum::{
    Json,
    extract::{Path, State},
};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::AppResult;

/// Recompute every derived value for a given derived parameter definition. Backfills via
/// joining source readings; tracked as a `reprocessing_jobs` row. Refreshes continuous
/// aggregates on completion. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/actions/derived_parameters/{id}/recompute",
    params(("id" = Uuid, Path, description = "Derived parameter definition UUID")),
    responses(
        (status = 200, description = "Background recompute job triggered with job_id"),
        (status = 404, description = "Derived parameter definition not found"),
    ),
    tag = "actions"
)]
pub async fn recompute_derived(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    let job_id = spawn_recompute_derived(&state.db, state.events.clone(), id).await?;
    Ok(Json(serde_json::json!({ "status": "queued", "job_id": job_id })))
}

/// Enqueue a durable `derived_recompute` job for one derived parameter definition. Shared by the
/// recompute action handler and the job-rerun dispatcher. Runs on the claim-based worker pool
/// (`DerivedRecompute`), reading `derived_definition_id` back from the job's params.
pub async fn spawn_recompute_derived(
    db: &sea_orm::DatabaseConnection,
    _events: crate::common::EventSender,
    id: Uuid,
) -> Result<Uuid, sea_orm::DbErr> {
    crate::routes::private::reprocessing_jobs::worker::enqueue(
        db,
        "derived_recompute",
        None,
        Some(id),
        &serde_json::json!({ "derived_definition_id": id }),
        None,
    )
    .await?
    .ok_or_else(|| sea_orm::DbErr::Custom("enqueue returned no id".into()))
}
