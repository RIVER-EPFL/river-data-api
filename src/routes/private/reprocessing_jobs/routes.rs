//! Custom (non-CrudCrate) endpoints for tracked jobs. Today: the per-job timeline feed.

use axum::Json;
use axum::extract::{Path, Query, State};
use sea_orm::{ConnectionTrait, Statement};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize)]
pub struct JobLogsQuery {
    /// Only return lines with `seq` strictly greater than this (incremental polling/tailing).
    #[serde(default)]
    pub after_seq: Option<i64>,
    /// Max lines to return (default 1000, capped at 5000).
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct JobLogLine {
    pub seq: i64,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub level: String,
    pub message: String,
    pub context: serde_json::Value,
}

/// `GET /api/reprocessing_jobs/{id}/logs` — the ordered timeline for one job. Paginated by `seq`
/// so the UI can lazy-load the full record and tail new lines. Requires `read_data`.
pub async fn get_job_logs(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<JobLogsQuery>,
) -> AppResult<Json<Vec<JobLogLine>>> {
    let limit = i64::try_from(q.limit.unwrap_or(1000).min(5000)).unwrap_or(1000);
    let after = q.after_seq.unwrap_or(-1);

    let rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT seq, ts, level, message, context \
             FROM reprocessing_job_logs \
             WHERE job_id = $1 AND seq > $2 \
             ORDER BY seq ASC LIMIT $3",
            [id.into(), after.into(), limit.into()],
        ))
        .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let ts: chrono::DateTime<chrono::FixedOffset> = r.try_get("", "ts")?;
        out.push(JobLogLine {
            seq: r.try_get("", "seq")?,
            ts: ts.with_timezone(&chrono::Utc),
            level: r.try_get("", "level")?,
            message: r.try_get("", "message")?,
            context: r.try_get("", "context")?,
        });
    }
    Ok(Json(out))
}

#[derive(Debug, Serialize)]
pub struct CancelResponse {
    pub status: String,
}

/// `POST /api/reprocessing_jobs/{id}/cancel` — cooperatively cancel a job. A `queued` job (not yet
/// claimed) is cancelled outright; a `running` worker-pool job is signalled via the `cancel_requested`
/// column, which the owning replica's heartbeat observes and the job honors at its next checkpoint.
/// `request_cancel` stays as a same-replica fast path for inline jobs. 409 if the type isn't
/// cancellable or the job isn't in a cancellable state; 404 if the id is unknown. Requires
/// `write_metadata`.
pub async fn cancel_job(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<CancelResponse>> {
    let row = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT trigger_type, status FROM reprocessing_jobs WHERE id = $1",
            [id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound(format!("job {id} not found")))?;

    let trigger_type: String = row.try_get("", "trigger_type")?;
    if !super::registry::is_cancellable(&trigger_type) {
        return Err(AppError::Conflict(format!(
            "jobs of type '{trigger_type}' cannot be cancelled once running"
        )));
    }

    // Set the durable flag so the owning replica — which may not be this one — stops the job at its
    // next checkpoint; a still-queued job is cancelled outright since nothing is running it yet.
    let flagged = state
        .db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE reprocessing_jobs \
             SET cancel_requested = true, \
                 status = CASE WHEN status = 'queued' THEN 'cancelled' ELSE status END, \
                 completed_at = CASE WHEN status = 'queued' THEN NOW() ELSE completed_at END \
             WHERE id = $1 AND status IN ('queued', 'pending', 'running', 'retrying')",
            [id.into()],
        ))
        .await?
        .rows_affected();

    // Same-replica fast path for inline jobs (no lease/heartbeat to observe the flag).
    let signalled_locally = super::lifecycle::request_cancel(id);

    if flagged > 0 || signalled_locally {
        Ok(Json(CancelResponse {
            status: "cancelling".to_string(),
        }))
    } else {
        Err(AppError::Conflict(
            "job is not in a cancellable state".to_string(),
        ))
    }
}

#[derive(Debug, Serialize)]
pub struct RerunResponse {
    pub job_id: Uuid,
    pub status: String,
}

/// `POST /api/reprocessing_jobs/{id}/rerun` — replay a finished job by reconstructing it from the
/// ids stored on its row. Returns a NEW job (history is preserved). 409 if the type isn't rerunnable
/// or an equivalent job is already in flight; 404 if the job id is unknown. Requires `write_metadata`.
pub async fn rerun_job(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<RerunResponse>> {
    let row = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT trigger_type, sensor_id, trigger_id, params FROM reprocessing_jobs WHERE id = $1",
            [id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound(format!("job {id} not found")))?;

    let trigger_type: String = row.try_get("", "trigger_type")?;
    let sensor_id: Option<Uuid> = row.try_get("", "sensor_id")?;
    let trigger_id: Option<Uuid> = row.try_get("", "trigger_id")?;
    let params: serde_json::Value = row.try_get("", "params").unwrap_or(serde_json::Value::Null);

    if !super::registry::is_rerunnable(&trigger_type) {
        return Err(AppError::Conflict(format!(
            "jobs of type '{trigger_type}' cannot be rerun"
        )));
    }

    // Reject if an equivalent job (same type + same target) is already in flight.
    let in_flight = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT 1 FROM reprocessing_jobs \
             WHERE status IN ('queued', 'pending', 'running', 'retrying') \
               AND trigger_type = $1 \
               AND sensor_id IS NOT DISTINCT FROM $2 \
               AND trigger_id IS NOT DISTINCT FROM $3 \
             LIMIT 1",
            [
                trigger_type.as_str().into(),
                sensor_id.map_or(sea_orm::Value::Uuid(None), Into::into),
                trigger_id.map_or(sea_orm::Value::Uuid(None), Into::into),
            ],
        ))
        .await?;
    if in_flight.is_some() {
        return Err(AppError::Conflict(
            "an equivalent job is already in flight".to_string(),
        ));
    }

    // Replay the original job from its persisted params: the row already carries the exact
    // `trigger_type`/`sensor_id`/`trigger_id`/`params` the first run used, so first-run and rerun
    // share the single leased `enqueue` path (no separate reconstruction, no inline spawn).
    let new_id = super::worker::enqueue(&state.db, &trigger_type, sensor_id, trigger_id, &params, None)
        .await?
        .ok_or_else(|| AppError::Conflict("an equivalent job is already in flight".to_string()))?;

    Ok(Json(RerunResponse {
        job_id: new_id,
        status: "queued".to_string(),
    }))
}
