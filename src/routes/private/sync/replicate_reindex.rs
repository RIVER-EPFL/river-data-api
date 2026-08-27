//! Operator surface of the replicate reindex repair: start the discard-and-refetch job for
//! replicate-family streams whose replicate indexes no longer name their source columns. The job
//! itself lives in `reprocessing_jobs::replicate_reindex`.

use axum::{Json, extract::State};
use sea_orm::{ConnectionTrait, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::reprocessing_jobs::{replicate_reindex, worker};

#[derive(Debug, Deserialize, ToSchema)]
pub struct StartReindexRepairRequest {
    /// The streams to repair. Either this or `source_system` is required.
    #[serde(default)]
    pub stream_ids: Vec<Uuid>,
    /// Repair every replicate family of a source, e.g. `cnet`.
    #[serde(default)]
    pub source_system: Option<String>,
    /// Report what would be discarded without deleting anything.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StartReindexRepairResponse {
    pub job_id: Uuid,
}

/// Start the repair job: for each targeted replicate-family stream, delete its readings, rewind
/// its sync cursor and ask the owning sync service to send the readings again. Destructive, and
/// refused outright when a targeted stream is paired to a site parameter or carries a flagged
/// reading.
#[utoipa::path(
    post,
    path = "/sync/replicate_reindex_repair",
    request_body = StartReindexRepairRequest,
    responses(
        (status = 200, body = StartReindexRepairResponse),
        (status = 400, description = "No scope given"),
        (status = 409, description = "A repair for this scope is already running"),
    ),
    tag = "sync"
)]
pub async fn start_reindex_repair(
    State(state): State<AppState>,
    Json(payload): Json<StartReindexRepairRequest>,
) -> AppResult<Json<StartReindexRepairResponse>> {
    let source_system = payload
        .source_system
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if payload.stream_ids.is_empty() && source_system.is_none() {
        return Err(AppError::BadRequest(
            "stream_ids or source_system is required".to_string(),
        ));
    }

    // One live run at a time: a second run over an overlapping scope would delete readings the
    // first has already asked the source to resend.
    let active = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM reprocessing_jobs
             WHERE trigger_type = $1 AND status IN ('queued', 'running', 'retrying')
             LIMIT 1",
            [replicate_reindex::TRIGGER_TYPE.into()],
        ))
        .await?;
    if let Some(row) = active {
        let id: Uuid = row.try_get("", "id")?;
        return Err(AppError::Conflict(format!(
            "{} already running (job {id})",
            replicate_reindex::TRIGGER_TYPE
        )));
    }

    let job_id = worker::enqueue(
        &state.db,
        replicate_reindex::TRIGGER_TYPE,
        None,
        None,
        &serde_json::json!({
            "stream_ids": payload.stream_ids,
            "source_system": source_system,
            "dry_run": payload.dry_run,
        }),
        None,
    )
    .await?
    .ok_or_else(|| AppError::Internal("job enqueue inserted nothing".to_string()))?;
    Ok(Json(StartReindexRepairResponse { job_id }))
}
