//! Operator surface of the replicate reconciliation: list what would migrate, start the
//! migrate+verify job, and (separately, after review) start the delete job. The jobs themselves
//! live in `reprocessing_jobs::reconcile`.

use axum::{
    Json,
    extract::{Query, State},
};
use sea_orm::{ConnectionTrait, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::reprocessing_jobs::{reconcile, worker};

#[derive(Debug, Deserialize, ToSchema)]
pub struct CandidatesQuery {
    pub source_system: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct FamilyCandidate {
    pub family_stream_id: Uuid,
    pub family_source_key: String,
    pub old_stream_id: Uuid,
    pub old_source_key: String,
    pub site_parameter_id: Option<Uuid>,
    pub migrated: bool,
    pub old_readings: i64,
    /// Old-stream instants the family stream has no readings for. Zero = ready for cutover.
    pub missing_instants: i64,
    pub ready: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CandidatesResponse {
    pub families: Vec<FamilyCandidate>,
    pub total_old_streams: usize,
}

/// The replicate families of a source and their migration state.
#[utoipa::path(
    get,
    path = "/sync/replicate_reconciliation/candidates",
    params(("source_system" = String, Query, description = "e.g. cnet")),
    responses((status = 200, body = CandidatesResponse)),
    tag = "sync"
)]
pub async fn reconciliation_candidates(
    State(state): State<AppState>,
    Query(query): Query<CandidatesQuery>,
) -> AppResult<Json<CandidatesResponse>> {
    let pairs = reconcile::family_pairs(&state.db, &query.source_system).await?;
    let mut families = Vec::with_capacity(pairs.len());
    for pair in &pairs {
        let row = state
            .db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT
                     (SELECT COUNT(*)::bigint FROM readings r
                      WHERE r.stream_id = $1 AND r.replicate_index = 0) AS old_readings,
                     (SELECT COUNT(*)::bigint FROM readings o
                      WHERE o.stream_id = $1 AND o.replicate_index = 0
                        AND NOT EXISTS (SELECT 1 FROM readings n
                                        WHERE n.stream_id = $2 AND n.time = o.time)) AS missing",
                [pair.old_id.into(), pair.new_id.into()],
            ))
            .await?
            .ok_or_else(|| AppError::Internal("candidate probe returned no row".to_string()))?;
        let old_readings: i64 = row.try_get("", "old_readings")?;
        let missing: i64 = row.try_get("", "missing")?;
        families.push(FamilyCandidate {
            family_stream_id: pair.new_id,
            family_source_key: pair.new_key.clone(),
            old_stream_id: pair.old_id,
            old_source_key: pair.old_key.clone(),
            site_parameter_id: pair.old_site_parameter_id,
            migrated: pair.new_paired,
            old_readings,
            missing_instants: missing,
            ready: !pair.new_paired && pair.old_site_parameter_id.is_some() && missing == 0,
        });
    }
    Ok(Json(CandidatesResponse {
        total_old_streams: families.len(),
        families,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct StartReconciliationRequest {
    pub source_system: String,
    #[serde(default)]
    pub dry_run: bool,
    /// Relative verification tolerance; defaults to the sync audit's.
    #[serde(default)]
    pub tolerance: Option<f64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StartReconciliationResponse {
    pub job_id: Uuid,
}

async fn enqueue_reconciliation(
    state: &AppState,
    trigger_type: &str,
    payload: &StartReconciliationRequest,
) -> AppResult<Json<StartReconciliationResponse>> {
    if payload.source_system.trim().is_empty() {
        return Err(AppError::BadRequest(
            "source_system is required".to_string(),
        ));
    }
    // One live run per (job kind, source): a second concurrent migration over the same streams
    // would race the per-family claims for no benefit.
    let active = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM reprocessing_jobs
             WHERE trigger_type = $1 AND status IN ('queued', 'running', 'retrying')
               AND params->>'source_system' = $2
             LIMIT 1",
            [trigger_type.into(), payload.source_system.clone().into()],
        ))
        .await?;
    if let Some(row) = active {
        let id: Uuid = row.try_get("", "id")?;
        return Err(AppError::Conflict(format!(
            "{trigger_type} already running for {} (job {id})",
            payload.source_system
        )));
    }

    let job_id = worker::enqueue(
        &state.db,
        trigger_type,
        None,
        None,
        &serde_json::json!({
            "source_system": payload.source_system,
            "dry_run": payload.dry_run,
            "tolerance": payload.tolerance,
        }),
        None,
    )
    .await?
    .ok_or_else(|| AppError::Internal("job enqueue inserted nothing".to_string()))?;
    Ok(Json(StartReconciliationResponse { job_id }))
}

/// Start the migrate + verify job. Non-destructive: pairs family streams to their slots and
/// materialises samples; a family failing verification rolls back untouched.
#[utoipa::path(
    post,
    path = "/sync/replicate_reconciliation",
    request_body = StartReconciliationRequest,
    responses(
        (status = 200, body = StartReconciliationResponse),
        (status = 409, description = "A reconciliation for this source is already running"),
    ),
    tag = "sync"
)]
pub async fn start_reconciliation(
    State(state): State<AppState>,
    Json(payload): Json<StartReconciliationRequest>,
) -> AppResult<Json<StartReconciliationResponse>> {
    enqueue_reconciliation(&state, "replicate_reconciliation", &payload).await
}

/// Start the delete job: re-verifies each migrated family and removes the obsolete avg streams
/// and their readings. The destructive step of the migration; run only after reviewing the
/// migrate job's verification report.
#[utoipa::path(
    post,
    path = "/sync/replicate_reconciliation/delete",
    request_body = StartReconciliationRequest,
    responses(
        (status = 200, body = StartReconciliationResponse),
        (status = 409, description = "A delete for this source is already running"),
    ),
    tag = "sync"
)]
pub async fn start_reconciliation_delete(
    State(state): State<AppState>,
    Json(payload): Json<StartReconciliationRequest>,
) -> AppResult<Json<StartReconciliationResponse>> {
    enqueue_reconciliation(&state, "replicate_reconciliation_delete", &payload).await
}
