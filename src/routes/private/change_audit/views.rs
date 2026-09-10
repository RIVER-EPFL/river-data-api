//! The subject-keyed reader.
//!
//! `change_audit` is one table for every trail of that shape: a schedule's edits, a parameter
//! group's membership, a parameter's or a slot's own columns. The key is the subject
//! (`schedule:{job_name}`, `parameter_group:{id}`, `parameter:{id}`, `site_parameter:{id}`), so one
//! query answers all of them and the per-entity routes are callers rather than second copies.

use axum::{
    Json,
    extract::{Query, State},
};
use serde::Deserialize;

use super::models::ChangeEntry;
use super::service::entries_for;
use crate::common::AppState;
use crate::error::AppResult;

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
