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
            "SELECT trigger_type, sensor_id, trigger_id FROM reprocessing_jobs WHERE id = $1",
            [id.into()],
        ))
        .await?
        .ok_or_else(|| AppError::NotFound(format!("job {id} not found")))?;

    let trigger_type: String = row.try_get("", "trigger_type")?;
    let sensor_id: Option<Uuid> = row.try_get("", "sensor_id")?;
    let trigger_id: Option<Uuid> = row.try_get("", "trigger_id")?;

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
             WHERE status IN ('pending', 'running', 'retrying') \
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

    let new_id = dispatch_rerun(&state, &trigger_type, sensor_id, trigger_id)
        .await?
        .ok_or_else(|| {
            AppError::Conflict("job is missing the data needed to rerun".to_string())
        })?;

    Ok(Json(RerunResponse {
        job_id: new_id,
        status: "pending".to_string(),
    }))
}

/// Reconstruct and spawn a fresh job equivalent to a rerunnable one, from its stored ids. Returns
/// `None` when a required id is absent (e.g. a sensor job with no `sensor_id`).
async fn dispatch_rerun(
    state: &AppState,
    trigger_type: &str,
    sensor_id: Option<Uuid>,
    trigger_id: Option<Uuid>,
) -> Result<Option<Uuid>, sea_orm::DbErr> {
    use crate::routes::private::sensor_calibrations::services::spawn_reprocessing_job;

    let job = match trigger_type {
        "refresh_aggregates" => spawn_refresh_job(state, false).await?,
        "refresh_aggregates_full" => spawn_refresh_job(state, true).await?,
        "derived_recompute" => match trigger_id {
            Some(def) => {
                crate::routes::private::admin::derived::spawn_recompute_derived(
                    &state.db,
                    state.events.clone(),
                    def,
                )
                .await?
            }
            None => return Ok(None),
        },
        // Everything else rerunnable is a sensor reprocess keyed by sensor_id.
        _ => match sensor_id {
            Some(sid) => {
                spawn_reprocessing_job(&state.db, sid, trigger_type, trigger_id, state.events.clone())
                    .await?
            }
            None => return Ok(None),
        },
    };
    Ok(Some(job))
}

async fn spawn_refresh_job(state: &AppState, full: bool) -> Result<Uuid, sea_orm::DbErr> {
    let trigger_type = if full {
        "refresh_aggregates_full"
    } else {
        "refresh_aggregates"
    };
    crate::routes::private::reprocessing_jobs::lifecycle::spawn_tracked_job(
        &state.db,
        None,
        trigger_type,
        None,
        state.events.clone(),
        move |db| async move {
            if full {
                crate::common::sync_state::refresh_continuous_aggregates_full(&db).await;
            } else {
                crate::common::sync_state::refresh_continuous_aggregates(&db, None).await;
            }
            Ok(0)
        },
    )
    .await
}
