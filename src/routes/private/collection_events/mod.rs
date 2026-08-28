//! Collection events: one row per (site, staged timestamp) visit — the portal's wide `data` row
//! as an entity (D7). Readings attach through `readings.collection_event_id`; the attach helper
//! in [`attach`] is the one place that link is written.

pub mod attach;
pub mod model;
pub use model::*;

use axum::{Json, extract::Path, extract::State};
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

#[derive(Debug, Serialize, ToSchema)]
pub struct EnqueuedJobResponse {
    pub job_id: Option<Uuid>,
}

/// Recompute a collection event's tool outputs on demand: the chain executor runs every active
/// tool whose inputs resolve at this event, in dependency order, and saves the outputs through
/// the grab write path with fresh server-built provenance. Tracked job. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/collection_events/{id}/recompute",
    params(("id" = Uuid, Path, description = "Collection event id")),
    responses(
        (status = 200, description = "The tracked recompute job", body = EnqueuedJobResponse),
        (status = 404, description = "Unknown collection event"),
    ),
    tag = "collection_events"
)]
pub async fn recompute_collection_event(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<EnqueuedJobResponse>> {
    let event = Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Collection event {id} not found")))?;
    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        &state.db,
        "event_recompute",
        None,
        Some(event.site_id),
        &serde_json::json!({
            "collection_event_id": id,
            "actor": crate::routes::private::tools::scripts::actor_label(&auth),
        }),
        None,
    )
    .await?;
    Ok(Json(EnqueuedJobResponse { job_id }))
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EventAuditRequest {
    /// Audit every event at this site. Omit both fields to audit every site.
    #[serde(default)]
    pub site_id: Option<Uuid>,
    /// Audit one event.
    #[serde(default)]
    pub collection_event_id: Option<Uuid>,
}

/// Run the missing/stale audit (D6): per collection event and active tool, report outputs missing
/// where the declared inputs exist, and outputs that disagree with a recompute under their pinned
/// script version. Findings land in the review queue (`replicate_audit_holds`, event kinds); the
/// auditor never writes values. Tracked job. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/actions/event_audit",
    request_body = EventAuditRequest,
    responses((status = 200, description = "The tracked audit job", body = EnqueuedJobResponse)),
    tag = "collection_events"
)]
pub async fn run_event_audit(
    State(state): State<AppState>,
    Json(req): Json<EventAuditRequest>,
) -> AppResult<Json<EnqueuedJobResponse>> {
    if let Some(id) = req.collection_event_id
        && Entity::find_by_id(id).one(&state.db).await?.is_none()
    {
        return Err(AppError::NotFound(format!("Collection event {id} not found")));
    }
    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        &state.db,
        "event_audit",
        None,
        req.site_id,
        &serde_json::json!({
            "site_id": req.site_id,
            "collection_event_id": req.collection_event_id,
        }),
        None,
    )
    .await?;
    Ok(Json(EnqueuedJobResponse { job_id }))
}

/// One event-audit finding as the review queue lists it.
#[derive(Debug, Serialize, ToSchema)]
pub struct EventAuditFinding {
    pub id: Uuid,
    /// `missing_output` or `stale_output`.
    pub kind: String,
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub collected_at: chrono::DateTime<chrono::Utc>,
    pub tool: Option<String>,
    pub status: String,
    #[schema(value_type = Object)]
    pub expected: serde_json::Value,
    #[schema(value_type = Object)]
    pub computed: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct EventAuditFindingsQuery {
    #[serde(default)]
    pub site_id: Option<Uuid>,
    /// Filter by status; defaults to `pending`.
    #[serde(default)]
    pub status: Option<String>,
}

/// List event-audit findings from the review queue. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/actions/event_audit_findings",
    params(EventAuditFindingsQuery),
    responses((status = 200, description = "Findings, newest first", body = [EventAuditFinding])),
    tag = "collection_events"
)]
pub async fn list_event_audit_findings(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<EventAuditFindingsQuery>,
) -> AppResult<Json<Vec<EventAuditFinding>>> {
    use sea_orm::ConnectionTrait;
    let status = query.status.as_deref().unwrap_or("pending").to_string();
    let mut sql = String::from(
        "SELECT id, kind, site_id, parameter_id, group_time, tool, status, expected, computed, \
         created_at FROM replicate_audit_holds WHERE stream_id IS NULL AND status = $1",
    );
    let mut binds: Vec<sea_orm::Value> = vec![status.into()];
    if let Some(site_id) = query.site_id {
        binds.push(site_id.into());
        sql.push_str(" AND site_id = $2");
    }
    sql.push_str(" ORDER BY created_at DESC LIMIT 500");
    let rows = state
        .db
        .query_all(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            binds,
        ))
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(EventAuditFinding {
            id: r.try_get("", "id")?,
            kind: r.try_get("", "kind")?,
            site_id: r.try_get("", "site_id")?,
            parameter_id: r.try_get("", "parameter_id")?,
            collected_at: r
                .try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "group_time")?
                .with_timezone(&chrono::Utc),
            tool: r.try_get("", "tool")?,
            status: r.try_get("", "status")?,
            expected: r.try_get("", "expected")?,
            computed: r.try_get("", "computed")?,
            created_at: r
                .try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "created_at")?
                .with_timezone(&chrono::Utc),
        });
    }
    Ok(Json(out))
}
