//! What was done to a thing, by whom, and when.
//!
//! `change_audit` is one table for every trail of that shape: a schedule's edits, a parameter
//! group's membership, a parameter's or a slot's own columns. The key is the subject
//! (`schedule:{job_name}`, `parameter_group:{id}`, `parameter:{id}`, `site_parameter:{id}`), so one
//! query answers all of them and the per-entity routes are callers rather than second copies.

use axum::{
    Json,
    extract::{Query, State},
};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::common::AppState;
use crate::error::AppResult;

/// The newest edits returned for one subject. A trail is read to answer a question about a
/// specific thing, so the cap is a page size rather than a limit anyone pages past.
const LIMIT: u64 = 100;

/// One entry as the API exposes it.
#[derive(Debug, Serialize, ToSchema, FromQueryResult)]
pub struct ChangeEntry {
    pub changed_at: chrono::DateTime<chrono::Utc>,
    #[schema(required)]
    pub changed_by: Option<String>,
    /// What happened, as the writer named it: `schedule_update`, `member_insert`, and so on.
    pub change: String,
    #[schema(value_type = Option<Object>)]
    #[schema(required)]
    pub old_value: Option<serde_json::Value>,
    #[schema(value_type = Option<Object>)]
    #[schema(required)]
    pub new_value: Option<serde_json::Value>,
}

/// Up to the 100 newest changes recorded against `subject`, newest first. An unknown subject is an
/// empty list, not a 404: nothing having been done to a thing is an answer.
pub async fn entries_for<C: ConnectionTrait>(db: &C, subject: &str) -> AppResult<Vec<ChangeEntry>> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT changed_at, changed_by, change, old_value, new_value \
             FROM change_audit WHERE subject = $1 \
             ORDER BY changed_at DESC LIMIT $2",
            [subject.into(), LIMIT.into()],
        ))
        .await?;
    rows.iter()
        .map(|r| Ok(ChangeEntry::from_query_result(r, "")?))
        .collect()
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct ChangeAuditQuery {
    /// The thing whose trail is wanted, as the writer keyed it.
    pub subject: String,
}

/// The change trail of one subject, newest first. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/change_audit",
    params(ChangeAuditQuery),
    responses((status = 200, description = "Changes, newest first", body = [ChangeEntry])),
    tag = "admin"
)]
pub async fn list_change_audit(
    State(state): State<AppState>,
    Query(q): Query<ChangeAuditQuery>,
) -> AppResult<Json<Vec<ChangeEntry>>> {
    Ok(Json(entries_for(&state.db, &q.subject).await?))
}
