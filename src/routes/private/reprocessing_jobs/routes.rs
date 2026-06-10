//! Custom (non-CrudCrate) endpoints for tracked jobs. Today: the per-job timeline feed.

use axum::Json;
use axum::extract::{Path, Query, State};
use sea_orm::{ConnectionTrait, Statement};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::AppResult;

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
