//! Operator surface of the replicate reconciliation: list what would migrate, start the
//! migrate+verify job, and (separately, after review) start the delete job. The jobs themselves
//! live in `reprocessing_jobs::reconcile`.

use axum::{
    Json,
    extract::{Query, State},
};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
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
    #[schema(required)]
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
    path = "/api/sync/replicate_reconciliation/candidates",
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
            .query_one_raw(Statement::from_sql_and_values(
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
        let ProbeCounts {
            old_readings,
            missing,
        } = ProbeCounts::from_query_result(&row, "")?;
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
        .query_one_raw(Statement::from_sql_and_values(
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
    path = "/api/sync/replicate_reconciliation",
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
    path = "/api/sync/replicate_reconciliation/delete",
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

/// The rows this file's raw queries return. Derived rather than hand-decoded so a column added to
/// a query and not to its reader is a compile error rather than a field silently left behind.
#[derive(FromQueryResult)]
struct ProbeCounts {
    old_readings: i64,
    missing: i64,
}

#[derive(FromQueryResult)]
struct SlotRow {
    site_parameter_id: Uuid,
    stream_id: Uuid,
    source_system: String,
    source_key: String,
    site_id: Uuid,
    site_name: String,
    parameter_id: Uuid,
    parameter_name: String,
}

#[derive(FromQueryResult)]
struct StreamExtent {
    readings: i64,
    first: Option<chrono::DateTime<chrono::Utc>>,
    last: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DuplicateSlotStream {
    pub stream_id: Uuid,
    pub source_system: String,
    pub source_key: String,
    pub readings: i64,
    #[schema(required)]
    pub first_reading: Option<chrono::DateTime<chrono::Utc>>,
    #[schema(required)]
    pub last_reading: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DuplicateSlot {
    pub site_id: Uuid,
    pub site_name: String,
    pub parameter_id: Uuid,
    pub parameter_name: String,
    pub site_parameter_id: Uuid,
    pub streams: Vec<DuplicateSlotStream>,
    /// Instants at this slot carrying readings from more than one stream.
    pub duplicated_instants: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DuplicateSlotsResponse {
    pub slots: Vec<DuplicateSlot>,
}

/// The (site, parameter) slots where two streams carry the same instant. Serving returns one row
/// per instant, so a duplicated slot is invisible on the chart; this is the list the operator
/// reconciles from. Two streams sharing a slot without ever sharing an instant (a sensor feed
/// beside a grab feed) is the normal case and is not listed.
#[utoipa::path(
    get,
    path = "/api/sync/replicate_reconciliation/duplicate_slots",
    responses((status = 200, body = DuplicateSlotsResponse)),
    tag = "sync"
)]
pub async fn duplicate_slots(State(state): State<AppState>) -> AppResult<Json<DuplicateSlotsResponse>> {
    // Duplication is a property of the pairing: a reading's site and parameter come from its
    // stream's slot, so two streams can only collide at an instant by sharing one.
    let rows = state
        .db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sp.id AS site_parameter_id, sp.site_id, sp.parameter_id, \
                    s.name AS site_name, p.name AS parameter_name, \
                    ds.id AS stream_id, ds.source_system, ds.source_key \
             FROM data_streams ds \
             JOIN site_parameters sp ON sp.id = ds.site_parameter_id \
             JOIN sites s ON s.id = sp.site_id \
             JOIN parameters p ON p.id = sp.parameter_id \
             WHERE sp.id IN ( \
                 SELECT site_parameter_id FROM data_streams \
                 WHERE site_parameter_id IS NOT NULL \
                 GROUP BY site_parameter_id HAVING COUNT(*) > 1 \
             ) \
             ORDER BY s.name, p.name, ds.source_key"
                .to_string(),
        ))
        .await?;

    let mut slots: Vec<DuplicateSlot> = Vec::new();
    for r in rows {
        let slot = SlotRow::from_query_result(&r, "")?;
        let (site_parameter_id, stream_id) = (slot.site_parameter_id, slot.stream_id);
        let stats = state
            .db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT COUNT(*)::bigint AS readings, MIN(time) AS first, MAX(time) AS last \
                 FROM readings WHERE stream_id = $1",
                [stream_id.into()],
            ))
            .await?
            .ok_or_else(|| AppError::Internal("stream probe returned no row".to_string()))?;
        let extent = StreamExtent::from_query_result(&stats, "")?;
        let stream = DuplicateSlotStream {
            stream_id,
            source_system: slot.source_system,
            source_key: slot.source_key,
            readings: extent.readings,
            first_reading: extent.first,
            last_reading: extent.last,
        };
        match slots.iter_mut().find(|s| s.site_parameter_id == site_parameter_id) {
            Some(slot) => slot.streams.push(stream),
            None => slots.push(DuplicateSlot {
                site_id: slot.site_id,
                site_name: slot.site_name,
                parameter_id: slot.parameter_id,
                parameter_name: slot.parameter_name,
                site_parameter_id,
                streams: vec![stream],
                duplicated_instants: 0,
            }),
        }
    }

    for slot in &mut slots {
        let row = state
            .db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT COUNT(*)::bigint AS c FROM ( \
                     SELECT time FROM readings \
                     WHERE site_id = $1 AND parameter_id = $2 AND withdrawn_at IS NULL \
                     GROUP BY time HAVING COUNT(DISTINCT stream_id) > 1 \
                 ) t",
                [slot.site_id.into(), slot.parameter_id.into()],
            ))
            .await?;
        slot.duplicated_instants = row
            .map(|r| r.try_get::<i64>("", "c"))
            .transpose()?
            .unwrap_or(0);
    }

    // A shared slot is only a duplicate once an instant actually carries both feeds.
    slots.retain(|s| s.duplicated_instants > 0);
    Ok(Json(DuplicateSlotsResponse { slots }))
}
