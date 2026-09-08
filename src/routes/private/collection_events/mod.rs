//! Collection events: one row per (site, staged timestamp) visit — the portal's wide `data` row
//! as an entity (D7). Readings attach through `readings.collection_event_id`; the attach helper
//! in [`attach`] is the one place that link is written.

pub mod attach;
pub mod model;
pub mod operations;
pub mod recompute;
pub mod visits;
pub use model::*;

use axum::{Json, extract::Path, extract::State};
use sea_orm::{EntityTrait, FromQueryResult};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

#[derive(Debug, Serialize, ToSchema)]
pub struct EnqueuedJobResponse {
    #[schema(required)]
    pub job_id: Option<Uuid>,
}

/// Recompute a collection event's tool outputs on demand: the chain executor runs every active
/// tool whose inputs resolve at this event, in dependency order, and saves the outputs through
/// the grab write path with fresh server-built provenance. Tracked job. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/collection_events/{id}/recompute",
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
            "actor": crate::common::actor::label(&auth),
        }),
        None,
    )
    .await?;
    Ok(Json(EnqueuedJobResponse { job_id }))
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StageEventRequest {
    pub site_id: Uuid,
    pub collected_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Serialize, ToSchema, sea_orm::FromQueryResult)]
pub struct StagedEvent {
    pub id: Uuid,
    pub site_id: Uuid,
    pub collected_at: chrono::DateTime<chrono::Utc>,
    pub source: String,
    #[schema(required)]
    pub created_by: Option<String>,
    #[schema(required)]
    pub notes: Option<String>,
    /// False when the visit already stood at this instant, so a second tool joins it.
    pub created: bool,
}

/// Stage a field visit: the portal's New Entry, made idempotent. A visit already standing at
/// `(site_id, collected_at)` is returned as it is, so two tools entering the same visit land on
/// one row instead of racing the unique key. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/collection_events/stage",
    request_body = StageEventRequest,
    responses(
        (status = 200, description = "The staged visit", body = StagedEvent),
        (status = 404, description = "Unknown site"),
    ),
    tag = "collection_events"
)]
pub async fn stage_collection_event(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(req): Json<StageEventRequest>,
) -> AppResult<Json<StagedEvent>> {
    use sea_orm::ConnectionTrait;

    if crate::routes::private::sites::Entity::find_by_id(req.site_id)
        .one(&state.db)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound(format!(
            "Site {} not found",
            req.site_id
        )));
    }

    let actor = crate::common::actor::label(&auth);
    let collected_at = sea_orm::prelude::DateTimeWithTimeZone::from(req.collected_at);
    let row = state
        .db
        .query_one_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "WITH staged AS (
                 INSERT INTO collection_events (site_id, collected_at, source, created_by, notes)
                 VALUES ($1, $2, 'manual', $3, $4)
                 ON CONFLICT (site_id, collected_at) DO NOTHING
                 RETURNING id, site_id, collected_at, source, created_by, notes, true AS created
             )
             SELECT * FROM staged
             UNION ALL
             SELECT id, site_id, collected_at, source, created_by, notes, false AS created
             FROM collection_events
             WHERE site_id = $1 AND collected_at = $2 AND NOT EXISTS (SELECT 1 FROM staged)",
            vec![
                req.site_id.into(),
                collected_at.into(),
                actor.into(),
                req.notes.into(),
            ],
        ))
        .await?
        .ok_or_else(|| AppError::Internal("Staging returned no visit".to_string()))?;

    Ok(Json(StagedEvent::from_query_result(&row, "")?))
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EventRecomputeRequest {
    /// Recompute every visit at this site.
    #[serde(default)]
    pub site_id: Option<Uuid>,
    /// Visits collected at or after this instant.
    #[serde(default)]
    pub start: Option<chrono::DateTime<chrono::Utc>>,
    /// Visits collected at or before this instant.
    #[serde(default)]
    pub end: Option<chrono::DateTime<chrono::Utc>>,
    /// Only visits with an open missing- or stale-output finding.
    #[serde(default)]
    pub only_findings: bool,
}

/// The scoped apply (ADR 0007): run the chain over every manual visit in a site and/or time
/// range, or over the visits with open event findings, in one tracked job. This is the repair
/// path for what the reactive hook does not see: a constant, a curve or a script activation. An
/// unbounded scope (no site, no range, not held to findings) is refused. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/event_recompute",
    request_body = EventRecomputeRequest,
    responses(
        (status = 200, description = "The tracked recompute job", body = EnqueuedJobResponse),
        (status = 400, description = "Unbounded scope, or end before start"),
        (status = 404, description = "Unknown site"),
    ),
    tag = "collection_events"
)]
pub async fn run_event_recompute(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(req): Json<EventRecomputeRequest>,
) -> AppResult<Json<EnqueuedJobResponse>> {
    let scope = crate::routes::private::tools::chain::RecomputeScope {
        site_id: req.site_id,
        start: req.start,
        end: req.end,
        only_findings: req.only_findings,
    };
    if !scope.is_bounded() {
        return Err(AppError::BadRequest(
            "A recompute needs a scope: a site, a time range, or only_findings".to_string(),
        ));
    }
    if let (Some(start), Some(end)) = (req.start, req.end)
        && end < start
    {
        return Err(AppError::BadRequest("end must be >= start".to_string()));
    }
    if let Some(site_id) = req.site_id
        && crate::routes::private::sites::Entity::find_by_id(site_id)
            .one(&state.db)
            .await?
            .is_none()
    {
        return Err(AppError::NotFound(format!("Site {site_id} not found")));
    }
    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        &state.db,
        "event_recompute",
        None,
        req.site_id,
        &serde_json::json!({
            "site_id": req.site_id,
            "start": req.start,
            "end": req.end,
            "only_findings": req.only_findings,
            "actor": crate::common::actor::label(&auth),
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
    path = "/api/actions/event_audit",
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
        return Err(AppError::NotFound(format!(
            "Collection event {id} not found"
        )));
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

