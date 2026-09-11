use axum::{
    Json,
    extract::{Query, State},
};
use sea_orm::Order;
use sea_orm::sea_query::{
    Alias, Condition, Expr, ExprTrait as _, Func, JoinType, PostgresQueryBuilder, Query as SeaQuery,
};
use sea_orm::{ColumnTrait, EntityTrait, FromQueryResult, QueryFilter, QueryOrder, QuerySelect};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::authz::AccessScope;
use crate::common::middleware::{DenyScoped, ProjectScope};
use crate::common::scope::{
    RowProject, Unowned, project_filter_sql, project_of_sensor, project_of_site,
    require_sites_in_scope, require_target_in_scope,
};
use crate::error::{AppError, AppResult};
use crate::routes::private::data_streams;
use crate::routes::private::parameters::models as parameters;
use crate::routes::private::readings::models as readings;
use crate::routes::private::readings::samples::models as samples;
use crate::routes::private::sensor_calibrations;
use crate::routes::private::sensor_calibrations::service::{
    bind_derived_variables, constants_named_by, evaluate_formula, recompute_deployed_until,
    site_property_values,
};
use crate::routes::private::sensor_deployments as deployments;
use crate::routes::private::sensor_deployments::flows as slots;
use crate::routes::private::standard_curves::models as standard_curves;
use crate::routes::private::sites::models as sites;
use crate::routes::private::site_parameters::models as site_parameters;
use crate::routes::private::sync::models::HoldKind;
use crate::routes::private::sync::models::HoldStatus;

/// The rows this file's raw queries return. Derived rather than hand-decoded so a column added to
/// a query and not to its reader is a compile error rather than a field silently left behind.
#[derive(FromQueryResult)]
struct SlotRow {
    sp_id: Uuid,
    parameter_id: Uuid,
    units: String,
}

#[derive(FromQueryResult)]
struct SeriesPoint {
    time: chrono::DateTime<chrono::FixedOffset>,
    val: f64,
}

#[derive(FromQueryResult)]
struct CandidateRow {
    sensor_id: Uuid,
    uncalibrated_count: i64,
    target_from: chrono::DateTime<chrono::FixedOffset>,
}

#[derive(FromQueryResult)]
struct ForeignCurveRow {
    sensor_id: Option<Uuid>,
    standard_curve_id: Uuid,
    curve_sensor_id: Uuid,
    curve_name: Option<String>,
    site_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
    n: i64,
    first_time: chrono::DateTime<chrono::FixedOffset>,
    last_time: chrono::DateTime<chrono::FixedOffset>,
}

// ---------------------------------------------------------------------------
// Project scope on the operator actions
//
// Three rules, applied by every action in this file:
//
// 1. A named site is confined with `require_sites_in_scope` (403), matching `preview_derived` and
//    the ingestion write paths.
// 2. A named row (a sensor, a deployment) is confined with `confine_target` (404 when no such row,
//    403 when it exists outside the caller's grants).
// 3. An action that names nothing runs against every project, so a restricted caller must name a
//    target: `require_named_target` refuses it. Administrators, unscoped tokens and sync tokens are
//    unrestricted and unaffected.
//
// `refresh_aggregates` and `reconcile_alarms` are the two exceptions to rule 3, and the reason is
// what they write: neither takes a target because neither touches stored measurements or history.
// They recompute derived state (the rollups, the open-alarm set) that the scheduler already
// recomputes on its own cadence, so a member triggering one changes nothing they could not obtain
// by waiting.
//
// `deny_scoped_token` on the route group stops a project-scoped API TOKEN before any of this; it
// was never a check on granted members, who reach these handlers as `AccessScope::Projects`.
// ---------------------------------------------------------------------------

/// Confine an action's named row to the caller's projects.
///
/// A row that does not exist is 404 for everyone, including an administrator: the action has
/// nothing to act on. A row outside a restricted caller's grants is 403, the same answer the route
/// already gives a project-scoped token, and the enumerations that could hand out such an id are
/// confined by the same scope.
fn confine_target(
    scope: &AccessScope,
    row: &RowProject,
    unowned: Unowned,
    what: &str,
) -> AppResult<()> {
    if matches!(row, RowProject::Missing) {
        return Err(AppError::NotFound(format!("{what} not found")));
    }
    require_target_in_scope(scope, row, unowned, what)
}

/// The sites the named deployments sit at, for confining a deployment-addressed action. An id that
/// resolves to no row contributes nothing; the action's own candidate selection reports it.
async fn deployment_sites(
    db: &sea_orm::DatabaseConnection,
    deployment_ids: &[Uuid],
) -> AppResult<Vec<Uuid>> {
    if deployment_ids.is_empty() {
        return Ok(Vec::new());
    }
    deployments::Entity::find()
        .select_only()
        .column(deployments::Column::SiteId)
        .distinct()
        .filter(deployments::Column::Id.is_in(deployment_ids.to_vec()))
        .into_tuple::<Uuid>()
        .all(db)
        .await
        .map_err(AppError::Database)
}

/// Refuse an untargeted run to a restricted caller: with nothing named, the action reaches every
/// project. `named` is whether the request identified something narrower than the whole
/// installation; `what` names what to pass instead. A request that names nothing *and* asks for
/// nothing keeps its existing 400, which is a bad request rather than a scope answer.
fn require_named_target(scope: &AccessScope, named: bool, what: &str) -> AppResult<()> {
    if named || !scope.is_restricted() {
        return Ok(());
    }
    Err(AppError::Forbidden(format!(
        "Name the {what} this action should touch; an unnamed target is not confined to your projects"
    )))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RefreshAggregatesRequest {
    /// Rematerialise only from this instant to now. Omitted, the whole history is rematerialised,
    /// which is the repair for a database edited out of band; the open bucket is served from the
    /// raw rows and each policy already covers the rest.
    #[serde(default)]
    pub since: Option<chrono::DateTime<chrono::Utc>>,
}

/// Refresh of TimescaleDB continuous aggregates, tracked as a `reprocessing_jobs` row.
/// Returns immediately with the job id; the refresh runs in a background task with a
/// 10-minute timeout (a timeout marks the job `failed`). Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/refresh_aggregates",
    request_body = RefreshAggregatesRequest,
    responses(
        (status = 200, description = "Refresh triggered", body = QueuedJobResponse),
    ),
    tag = "actions"
)]
pub async fn refresh_aggregates(
    State(app_state): State<AppState>,
    Json(payload): Json<RefreshAggregatesRequest>,
) -> AppResult<Json<QueuedJobResponse>> {
    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        &app_state.db,
        "refresh_aggregates",
        None,
        None,
        &serde_json::json!({ "since": payload.since.map(|t| t.to_rfc3339()) }),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(QueuedJobResponse::queued(job_id)))
}

/// What a computation request enqueues: the job, and how many instants it covers.
#[derive(Debug, Serialize, ToSchema)]
pub struct ComputeDerivedResponse {
    /// The row enqueued, or null where an identical job was already queued.
    #[schema(required)]
    pub job_id: Option<Uuid>,
    /// `queued`, always.
    pub status: String,
    pub total_timestamps: usize,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ComputeDerivedRequest {
    pub site_timestamps: Vec<SiteTimestamps>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SiteTimestamps {
    pub site_id: Uuid,
    pub timestamps: Vec<chrono::DateTime<chrono::Utc>>,
}

/// Compute and upsert derived parameter values for the given (site, timestamp) pairs,
/// tracked as a `reprocessing_jobs` row (`readings_updated` = computed count). Runs derived
/// formula evaluation against source readings, then refreshes aggregates. Returns the job id
/// immediately. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/compute_derived",
    request_body = ComputeDerivedRequest,
    responses(
        (status = 200, description = "Computation triggered", body = ComputeDerivedResponse),
        (status = 403, description = "A named site is outside the caller's projects, or no site was named"),
    ),
    tag = "actions"
)]
pub async fn compute_derived(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<ComputeDerivedRequest>,
) -> AppResult<Json<ComputeDerivedResponse>> {
    let sites: Vec<Uuid> = payload
        .site_timestamps
        .iter()
        .map(|st| st.site_id)
        .collect();
    require_named_target(&scope, !sites.is_empty(), "site")?;
    require_sites_in_scope(&app_state.db, &scope, &sites).await?;

    let total_timestamps: usize = payload
        .site_timestamps
        .iter()
        .map(|st| st.timestamps.len())
        .sum();

    let site_timestamps: Vec<serde_json::Value> = payload
        .site_timestamps
        .iter()
        .map(|st| {
            serde_json::json!({
                "site_id": st.site_id,
                "timestamps": st.timestamps,
            })
        })
        .collect();

    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        &app_state.db,
        "compute_derived",
        None,
        None,
        &serde_json::json!({ "site_timestamps": site_timestamps }),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(ComputeDerivedResponse {
        job_id,
        status: "queued".to_string(),
        total_timestamps,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ReprocessSensorRequest {
    pub sensor_id: Uuid,
}

/// Re-derive `calibration_id`, `deployment_id`/`site_id`, and `calibrated_value` for every
/// reading owned by a sensor from its calibration and deployment windows, cascade to derived
/// parameters, and refresh aggregates. Tracked as a `reprocessing_jobs` row; returns the job
/// id immediately. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/actions/reprocess",
    request_body = ReprocessSensorRequest,
    responses(
        (status = 200, description = "Reprocessing triggered", body = QueuedJobResponse),
        (status = 403, description = "The sensor is deployed only outside the caller's projects"),
        (status = 404, description = "No such sensor"),
    ),
    tag = "actions"
)]
pub async fn reprocess_sensor(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<ReprocessSensorRequest>,
) -> AppResult<Json<QueuedJobResponse>> {
    let sensor_id = payload.sensor_id;
    // A sensor that has never been deployed belongs to no project, and neither do its readings, so
    // it stays reachable: an instrument sits in inventory before anyone decides where it goes.
    let target = project_of_sensor(&app_state.db, sensor_id).await?;
    confine_target(&scope, &target, Unowned::Allow, "sensor")?;

    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        &app_state.db,
        "manual_reprocess",
        Some(sensor_id),
        None,
        &serde_json::json!({ "sensor_id": sensor_id }),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(QueuedJobResponse::queued(job_id)))
}

/// One backdate is in flight at a time, so every request carries the same key.
const REPROCESS_ALL_DEDUPE_KEY: &str = "reprocess_all";

/// What one reconciliation pass changed.
#[derive(Debug, Serialize, ToSchema)]
pub struct ReconcileAlarmsResponse {
    pub opened: usize,
    pub updated: usize,
    pub resolved: usize,
}

/// What a route that enqueues one job answers with: the row it enqueued, and the state that row is
/// in when the response is written.
#[derive(Debug, Serialize, ToSchema)]
pub struct QueuedJobResponse {
    /// The row enqueued, or null where an identical job was already queued under the same dedupe
    /// key and this request added none.
    #[schema(required)]
    pub job_id: Option<Uuid>,
    /// `queued`, always.
    pub status: String,
}

impl QueuedJobResponse {
    pub(crate) fn queued(job_id: Option<Uuid>) -> Self {
        Self {
            job_id,
            status: "queued".to_string(),
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReprocessAllResponse {
    pub job_id: Uuid,
    pub status: String,
    /// Number of (site, parameter) slots queued for re-derivation.
    pub slots: usize,
}

/// Re-derive `sensor_id`/`deployment_id`/`site_id`/`calibrated_value` for ALL historical readings
/// from the current deployment + calibration timelines, across every `(site, parameter)` slot that
/// has a deployment. Use after correcting deployment/calibration windows in bulk (the backdate of
/// historical attribution). Each slot is reprocessed via the decompression-safe
/// `reprocess_site_parameter_readings`; runs as one tracked job. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/reprocess_all",
    responses(
        (status = 200, description = "Backdate reprocessing triggered; returns job_id and slot count", body = ReprocessAllResponse),
        (status = 403, description = "The backdate names no target, so a caller confined to a project set is refused"),
    ),
    tag = "actions"
)]
pub async fn reprocess_all(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<ReprocessAllResponse>> {
    use sea_orm::ConnectionTrait;

    // The backdate has no target field at all: it re-derives every slot in the installation.
    require_named_target(&scope, false, "sensor (POST /actions/reprocess)")?;

    let db = &app_state.db;
    // Count the slots only to report it back synchronously; the job re-reads `sensor_deployments`
    // itself, so a rerun reflects the current topology.
    let slot_count = deployments::Entity::find()
        .select_only()
        .column(deployments::Column::SiteId)
        .column(deployments::Column::ParameterId)
        .distinct()
        .into_tuple::<(Uuid, Uuid)>()
        .all(db)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
        .len();

    // One backdate at a time: a second request while one is queued joins it rather than starting a
    // concurrent pass over every slot. The claim releases the key, so a run already under way still
    // gets one follow-up.
    let queued = crate::routes::private::reprocessing_jobs::worker::enqueue(
        db,
        "reprocess_all",
        None,
        None,
        &serde_json::json!({}),
        Some(REPROCESS_ALL_DEDUPE_KEY),
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    let job_id = match queued {
        Some(id) => id,
        // The enqueue coalesced onto a run already queued under this key; that run is the answer.
        None => crate::routes::private::reprocessing_jobs::model::Entity::find()
            .filter(crate::routes::private::reprocessing_jobs::model::Column::DedupeKey.eq(REPROCESS_ALL_DEDUPE_KEY))
            .one(db)
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
            .ok_or_else(|| AppError::Internal("failed to enqueue reprocess_all job".to_string()))?
            .id,
    };

    Ok(Json(ReprocessAllResponse {
        job_id,
        status: "queued".to_string(),
        slots: slot_count,
    }))
}

#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct RebuildAlarmEventsRequest {
    /// Restrict to one site (default: every active site).
    #[serde(default)]
    pub site_id: Option<Uuid>,
    /// Restrict to one parameter (default: every parameter at the targeted sites).
    #[serde(default)]
    pub parameter_id: Option<Uuid>,
    /// Window start (ISO 8601). Defaults per-slot to the slot's earliest reading.
    #[serde(default)]
    pub start: Option<chrono::DateTime<chrono::Utc>>,
    /// Window end (ISO 8601). Defaults per-slot to the slot's latest reading.
    #[serde(default)]
    pub end: Option<chrono::DateTime<chrono::Utc>>,
}

/// Reconstruct persisted alarm events from the actual readings, for the targeted slots and window.
/// Walks the readings, collapses consecutive out-of-range readings into resolved breach episodes,
/// and writes them to `alarm_events` (idempotently). This is the on-demand twin of the automatic
/// backfill that fires after a CSV import / batch ingest; the live 60s sweeper still owns currently
/// open breaches. Tracked as a `reprocessing_jobs` row (`trigger_type = 'alarm_backfill'`); returns
/// the job id immediately. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/rebuild_alarm_events",
    request_body = RebuildAlarmEventsRequest,
    responses(
        (status = 200, description = "Rebuild triggered", body = QueuedJobResponse),
        (status = 403, description = "The named site is outside the caller's projects, or no site was named"),
    ),
    tag = "actions"
)]
pub async fn rebuild_alarm_events(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<RebuildAlarmEventsRequest>,
) -> AppResult<Json<QueuedJobResponse>> {
    let RebuildAlarmEventsRequest {
        site_id,
        parameter_id,
        start,
        end,
    } = payload;

    require_named_target(&scope, site_id.is_some(), "site")?;
    if let Some(site_id) = site_id {
        require_sites_in_scope(&app_state.db, &scope, &[site_id]).await?;
    }

    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        &app_state.db,
        "alarm_backfill",
        None,
        None,
        &serde_json::json!({
            "site_id": site_id,
            "parameter_id": parameter_id,
            "start": start,
            "end": end,
        }),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(QueuedJobResponse::queued(job_id)))
}

/// Force a full open-alarm reconcile right now, instead of waiting for the periodic backstop
/// sweep. Runs the same single tick the sweeper runs (open new breaches, refresh still-breaching,
/// auto-resolve returned-to-range) across every active slot, synchronously, the post-LATERAL
/// breach query is O(active slots), so this returns in well under a second. Operator escape hatch
/// for "I changed something and want the alarm state correct immediately". Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/reconcile_alarms",
    responses(
        (
            status = 200,
            description = "Reconcile complete; counts of opened/updated/resolved events",
            body = ReconcileAlarmsResponse
        ),
    ),
    tag = "actions"
)]
pub async fn reconcile_alarms(
    State(app_state): State<AppState>,
) -> AppResult<Json<ReconcileAlarmsResponse>> {
    let stats = crate::routes::private::alarms::flows::evaluate_alarm_events(&app_state.db).await?;

    if stats.opened > 0 || stats.resolved > 0 {
        let _ = app_state
            .events
            .send(crate::common::AppEvent::AlarmStateChanged {
                opened: stats.opened,
                resolved: stats.resolved,
            });
    }

    Ok(Json(ReconcileAlarmsResponse {
        opened: stats.opened,
        updated: stats.updated,
        resolved: stats.resolved,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RollbackDeploymentRequest {
    pub deployment_id: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RollbackDeploymentResponse {
    pub status: String,
    pub readings_reassigned: u64,
    #[schema(required)]
    pub previous_deployment_id: Option<Uuid>,
    /// The reprocess this rollback queued: the re-derivation runs as a tracked job, like every
    /// other caller's, rather than holding the request open for the whole cascade.
    pub job_id: Uuid,
}

/// Undo the most recent sensor deployment, reassigning its readings back to the previous
/// deployment's site. Used after an accidentally-created deployment. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/rollback_deployment",
    request_body = RollbackDeploymentRequest,
    responses(
        (status = 200, description = "Rollback complete with reassignment count", body = RollbackDeploymentResponse),
        (status = 403, description = "The deployment is outside the caller's projects"),
        (status = 404, description = "Deployment not found"),
    ),
    tag = "actions"
)]
pub async fn rollback_deployment(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<RollbackDeploymentRequest>,
) -> AppResult<Json<RollbackDeploymentResponse>> {
    use sea_orm::TransactionTrait;

    let db = &app_state.db;

    // 1. Load the target deployment
    //
    // A deployment is always at a site, so its project is the site's. This deletes the row, the
    // same destruction `DELETE /sensor_deployments/{id}` performs under `inject_project_scope`.
    // `deployed_until` is the boundary the rolled-back deployment vacates; the previous one
    // re-extends to it (NULL = the target was open-ended, so the previous reopens open-ended too).
    let target = deployments::Entity::find_by_id(payload.deployment_id)
        .one(db)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
        .ok_or_else(|| AppError::NotFound("Deployment not found".into()))?;
    let owner = project_of_site(db, target.site_id).await?;
    confine_target(&scope, &owner, Unowned::Deny, "deployment")?;

    let sensor_id = target.sensor_id;
    let parameter_id = target.parameter_id;
    let target_deployed_from = target.deployed_from;
    let target_deployed_until = target.deployed_until;

    // 2. Find the previous deployment for the same sensor AND THE SAME PARAMETER, on a multi-channel
    //    instrument the immediately-prior deployment by time could belong to a different channel;
    //    reopening that one would extend the wrong channel's window.
    let previous = deployments::Entity::find()
        .filter(deployments::Column::SensorId.eq(sensor_id))
        .filter(deployments::Column::ParameterId.eq(parameter_id))
        .filter(deployments::Column::DeployedFrom.lt(target_deployed_from))
        .filter(deployments::Column::Id.ne(payload.deployment_id))
        .order_by_desc(deployments::Column::DeployedFrom)
        .one(db)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
    let previous_deployment_id: Option<Uuid> = previous.as_ref().map(|p| p.id);

    // 3-4. Clear readings' FK to the rolled-back deployment, delete it, and reopen the previous
    //    deployment, atomically, so a mid-operation failure can't leave the deployment deleted with
    //    nothing reopened (which would silently un-attribute its readings). The decompression cap is
    //    lifted so the readings FK-clear can't fail on old compressed chunks.
    let txn = db
        .begin()
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
    crate::common::bulk_write::lift_decompression_cap(&txn).await?;

    let readings_reassigned = readings::Entity::update_many()
        .col_expr(
            readings::Column::DeploymentId,
            Expr::value(Option::<Uuid>::None),
        )
        .filter(readings::Column::DeploymentId.eq(payload.deployment_id))
        .exec(&txn)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
        .rows_affected;

    deployments::Entity::delete_by_id(payload.deployment_id)
        .exec(&txn)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    // Reopen the previous deployment to absorb the vacated window. `recompute_deployed_until` only ever
    //    SHORTENS (LEAST), so without this the previous deployment, auto-closed when the rolled-back
    //    one was created, stays closed and the readings would un-attribute (site_id NULL) instead of
    //    reverting to the previous site. Reopening to the target's own `deployed_until` reclaims exactly
    //    the window the target held (NULL = open-ended), which can't overlap anything the slot
    //    constraint already excluded.
    //    Another instrument may have moved into the window the predecessor is about to reclaim. The
    //    check runs after the DELETE above so the deployment being rolled back is not itself
    //    reported as the occupant; a conflict aborts the transaction and nothing is destroyed.
    if let Some(prev) = &previous {
        let prev_id = prev.id;
        let prev_site = prev.site_id;
        let prev_from = prev.deployed_from;

        let request = slots::SlotRequest {
            site_id: prev_site,
            parameter_id,
            deployed_from: prev_from.with_timezone(&chrono::Utc),
            deployed_until: target_deployed_until.map(|t| t.with_timezone(&chrono::Utc)),
            exclude_deployment: Some(prev_id),
            recalled_sensor: None,
        };
        if let Some(occupant) = slots::find_occupant(&txn, &request)
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
        {
            return Err(AppError::Conflict(slots::conflict_message(
                &occupant,
                sensor_id,
                "roll back",
            )));
        }

        deployments::Entity::update_many()
            .col_expr(
                deployments::Column::DeployedUntil,
                Expr::value(target_deployed_until),
            )
            .filter(deployments::Column::Id.eq(prev_id))
            .exec(&txn)
            .await
            .map_err(|e| {
                if slots::is_slot_conflict(&e) {
                    AppError::Conflict(format!(
                        "Rolling back would extend deployment {prev_id} into a period another \
                     instrument now holds at this site and parameter."
                    ))
                } else {
                    AppError::Internal(format!("DB error: {e}"))
                }
            })?;
    }
    txn.commit()
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    // 5. Re-chain the remaining timeline and re-derive every reading for the sensor by window. The
    //    rolled-back deployment's readings now fall in the reopened previous deployment's window (or
    //    in a gap → no site, if there was no previous). Re-chaining only ever shortens, so it can't
    //    violate the slot-exclusion constraint. Reprocess also refreshes the continuous aggregates.
    recompute_deployed_until(db, sensor_id)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        db,
        "manual_reprocess",
        Some(sensor_id),
        None,
        &serde_json::json!({ "sensor_id": sensor_id }),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?
    .ok_or_else(|| AppError::Internal("failed to enqueue the rollback reprocess".to_string()))?;

    tracing::info!(
        deployment_id = %payload.deployment_id,
        sensor_id = %sensor_id,
        readings_reassigned,
        previous = ?previous_deployment_id,
        "Rolled back deployment"
    );

    Ok(Json(RollbackDeploymentResponse {
        status: "rolled_back".to_string(),
        readings_reassigned,
        previous_deployment_id,
        job_id,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PreviewDerivedRequest {
    pub formula: String,
    pub site_id: Uuid,
    pub start: chrono::DateTime<chrono::Utc>,
    pub end: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PreviewDerivedResponse {
    pub site: PreviewSite,
    pub times: Vec<chrono::DateTime<chrono::Utc>>,
    pub source_parameters: Vec<SourceParameterSeries>,
    pub derived: DerivedSeries,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PreviewSite {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SourceParameterSeries {
    pub name: String,
    pub units: String,
    pub values: Vec<Option<f64>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DerivedSeries {
    pub name: String,
    pub formula: String,
    pub values: Vec<Option<f64>>,
    pub errors: Vec<Option<String>>,
}

/// Preview a derived parameter formula against historical source readings at a given site,
/// WITHOUT writing anything to the database. Used by the formula builder UI to validate
/// formulas before saving. Requires `read_data`.
#[utoipa::path(
    post,
    path = "/api/actions/preview_derived",
    request_body = PreviewDerivedRequest,
    responses(
        (status = 200, description = "Computed values with per-timestamp errors", body = PreviewDerivedResponse),
        (status = 400, description = "Invalid formula syntax or unknown variables"),
        (status = 404, description = "Site not found"),
    ),
    tag = "actions"
)]
pub async fn preview_derived(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<PreviewDerivedRequest>,
) -> AppResult<Json<PreviewDerivedResponse>> {
    use sea_orm::{ConnectionTrait, Statement};

    // A restricted caller may only preview a derived computation against a site in its projects.
    require_sites_in_scope(&app_state.db, &scope, &[payload.site_id]).await?;

    // Validate formula
    if payload.formula.len() > 1000 {
        return Err(AppError::BadRequest(
            "Formula too long (max 1000 characters)".to_string(),
        ));
    }
    payload
        .formula
        .parse::<meval::Expr>()
        .map_err(|e| AppError::BadRequest(format!("Invalid formula: {e}")))?;

    let db = &app_state.db;

    // Get site name
    let site_name = crate::routes::private::sites::Entity::find_by_id(payload.site_id)
        .one(db)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
        .ok_or_else(|| AppError::NotFound("Site not found".into()))?
        .name;

    // Extract variable names from formula
    let var_names = crate::routes::private::tools::service::free_identifiers(&payload.formula);

    if var_names.is_empty() {
        return Ok(Json(PreviewDerivedResponse {
            site: PreviewSite {
                id: payload.site_id,
                name: site_name,
            },
            times: vec![],
            source_parameters: vec![],
            derived: DerivedSeries {
                name: "preview".to_string(),
                formula: payload.formula,
                values: vec![],
                errors: vec![],
            },
        }));
    }

    // Resolve variable names → site_parameters at this site
    let mut param_info: Vec<(String, Uuid, Uuid, String)> = Vec::new();

    for var_name in &var_names {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT sp.id as sp_id, sp.parameter_id, COALESCE(sp.display_units, '') as units
                  FROM site_parameters sp
                  JOIN parameters pt ON pt.id = sp.parameter_id
                  WHERE sp.site_id = $1 AND pt.code = $2
                  LIMIT 1",
                [payload.site_id.into(), var_name.clone().into()],
            ))
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

        if let Some(row) = row {
            let SlotRow {
                sp_id,
                parameter_id,
                units,
            } = SlotRow::from_query_result(&row, "")
                .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
            param_info.push((var_name.clone(), sp_id, parameter_id, units));
        }
    }

    // A name that is not a slot at this site is a constant or a column of the site's row, bound
    // the way the continuous path binds them.
    let declared: Vec<String> = param_info.iter().map(|(name, ..)| name.clone()).collect();
    let constants = constants_named_by(db, &payload.formula, &declared)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
    let properties: Vec<(String, String)> = var_names
        .iter()
        .filter(|name| !declared.contains(name) && !constants.contains_key(name.as_str()))
        .map(|name| (name.clone(), name.clone()))
        .collect();
    let site_properties = site_property_values(db, payload.site_id, &properties)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    // Fetch readings for all resolved parameters within time range
    let mut all_times: Vec<chrono::DateTime<chrono::Utc>> = Vec::new();
    let mut source_data: HashMap<String, HashMap<i64, f64>> = HashMap::new();
    let mut source_units: HashMap<String, String> = HashMap::new();

    for (var_name, _sp_id, parameter_id, units) in &param_info {
        source_units.insert(var_name.clone(), units.clone());

        let r = Alias::new("r");
        let smp = Alias::new("smp");
        let (sql, values) = SeaQuery::select()
            .distinct_on([(r.clone(), readings::Column::Time)])
            .column((r.clone(), readings::Column::Time))
            .expr_as(crate::common::served::spot_value(), Alias::new("val"))
            .from_as(readings::Entity, r.clone())
            .join_as(
                JoinType::LeftJoin,
                samples::Entity,
                smp.clone(),
                Expr::col((smp.clone(), samples::Column::Id))
                    .equals((r.clone(), readings::Column::SampleId)),
            )
            .and_where(Expr::col((r.clone(), readings::Column::ParameterId)).eq(*parameter_id))
            .and_where(Expr::col((r.clone(), readings::Column::SiteId)).eq(payload.site_id))
            .and_where(Expr::col((r.clone(), readings::Column::Time)).gte(payload.start))
            .and_where(Expr::col((r.clone(), readings::Column::Time)).lte(payload.end))
            .order_by((r.clone(), readings::Column::Time), Order::Asc)
            .order_by_expr(
                Expr::cust("(r.measurement_type IS NOT DISTINCT FROM 'spot')"),
                Order::Asc,
            )
            .order_by((r.clone(), readings::Column::StreamId), Order::Asc)
            .order_by_expr(Expr::cust("(r.is_flagged IS TRUE)"), Order::Asc)
            .order_by((r.clone(), readings::Column::ReplicateIndex), Order::Asc)
            .take()
            .build(PostgresQueryBuilder);
        let rows = db
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                sql,
                values,
            ))
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

        let map = source_data.entry(var_name.clone()).or_default();
        for row in rows {
            let SeriesPoint { time, val } = SeriesPoint::from_query_result(&row, "")
                .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
            let utc = time.with_timezone(&chrono::Utc);
            map.insert(utc.timestamp_millis(), val);
            all_times.push(utc);
        }
    }

    // Deduplicate and sort times
    let mut time_set: Vec<i64> = all_times
        .iter()
        .map(chrono::DateTime::timestamp_millis)
        .collect();
    time_set.sort_unstable();
    time_set.dedup();

    let times: Vec<chrono::DateTime<chrono::Utc>> = time_set
        .iter()
        .map(|ms| chrono::DateTime::from_timestamp_millis(*ms).unwrap_or_default())
        .collect();

    // Build source parameter series
    let source_parameters: Vec<SourceParameterSeries> = var_names
        .iter()
        .filter(|vn| param_info.iter().any(|(n, _, _, _)| n == *vn))
        .map(|var_name| {
            let data = source_data.get(var_name);
            let units = source_units.get(var_name).cloned().unwrap_or_default();
            let values: Vec<Option<f64>> = time_set
                .iter()
                .map(|ms| data.and_then(|d| d.get(ms).copied()))
                .collect();
            SourceParameterSeries {
                name: var_name.clone(),
                units,
                values,
            }
        })
        .collect();

    // Evaluate formula at each timestamp
    let mut derived_values: Vec<Option<f64>> = Vec::with_capacity(times.len());
    let mut derived_errors: Vec<Option<String>> = Vec::with_capacity(times.len());

    for ms in &time_set {
        let parameters: Vec<(String, Option<f64>)> = declared
            .iter()
            .map(|name| {
                let value = source_data.get(name).and_then(|data| data.get(ms)).copied();
                (name.clone(), value)
            })
            .collect();
        let Ok(vars) =
            bind_derived_variables(&payload.formula, &parameters, &site_properties, &constants)
        else {
            derived_values.push(None);
            derived_errors.push(None);
            continue;
        };

        match evaluate_formula(&payload.formula, &vars) {
            Ok(val) if val.is_finite() => {
                derived_values.push(Some(val));
                derived_errors.push(None);
            }
            Ok(val) => {
                derived_values.push(None);
                derived_errors.push(Some(format!("Non-finite result: {val}")));
            }
            Err(e) => {
                derived_values.push(None);
                derived_errors.push(Some(e));
            }
        }
    }

    Ok(Json(PreviewDerivedResponse {
        site: PreviewSite {
            id: payload.site_id,
            name: site_name,
        },
        times,
        source_parameters,
        derived: DerivedSeries {
            name: "preview".to_string(),
            formula: payload.formula,
            values: derived_values,
            errors: derived_errors,
        },
    }))
}

// ---------------------------------------------------------------------------
// Bulk historical attribution (backfill)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema, Clone)]
pub struct BackfillCandidate {
    pub deployment_id: Uuid,
    pub sensor_id: Uuid,
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    /// The deployment's current start.
    pub deployed_from: chrono::DateTime<chrono::Utc>,
    /// Earliest claimable unattributed reading, the date `deployed_from` would move back to.
    pub target_from: chrono::DateTime<chrono::Utc>,
    /// Number of unattributed readings (`sensor_id IS NULL`) in `[target_from, deployed_from)`.
    pub claimable_count: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BackfillSiteSummary {
    pub site_id: Uuid,
    pub deployments: i64,
    pub claimable_count: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BackfillCandidatesResponse {
    pub candidates: Vec<BackfillCandidate>,
    pub by_site: Vec<BackfillSiteSummary>,
    pub total_candidates: usize,
    pub total_claimable: i64,
}

/// Open deployments that have claimable pre-start history: readings at the same `(site, parameter)`
/// with `sensor_id IS NULL` before `deployed_from`, bounded below by any prior deployment's end so
/// backdating can't overlap it. `target_from` is the earliest such reading.
///
/// Confined to the caller's projects. The rows carry the site, sensor and deployment ids the write
/// actions in this file take, so an unconfined enumeration hands out exactly what a cross-project
/// write needs; the CRUD reads of the same rows confine by the same project set.
async fn fetch_backfill_candidates(
    db: &sea_orm::DatabaseConnection,
    scope: &AccessScope,
) -> AppResult<Vec<BackfillCandidate>> {
    use sea_orm::{ConnectionTrait, Statement};
    let d = Alias::new("d");
    let s_ = Alias::new("s");
    let pe = Alias::new("pe");
    let c = Alias::new("c");
    let r = Alias::new("r");
    let p = Alias::new("p");

    let prior_end = SeaQuery::select()
        .expr_as(
            Func::max(Expr::col((p.clone(), deployments::Column::DeployedUntil))),
            Alias::new("prior_end"),
        )
        .from_as(deployments::Entity, p.clone())
        .and_where(Expr::cust("p.site_id = d.site_id"))
        .and_where(Expr::cust("p.parameter_id = d.parameter_id"))
        .and_where(Expr::cust("p.id <> d.id"))
        .and_where(Expr::col((p.clone(), deployments::Column::DeployedUntil)).is_not_null())
        .and_where(Expr::cust("p.deployed_until <= d.deployed_from"))
        .take();

    // Claimable is "no deployment covers this reading", not "no instrument names it": every
    // non-derived row names an instrument from the moment it is written
    // (`readings_instrument_required`), and it is the deployment the backdate supplies. A derived
    // value carries the slot and no instrument by design, so it is never claimed.
    let claimable = SeaQuery::select()
        .expr_as(
            Func::min(Expr::col((r.clone(), readings::Column::Time))),
            Alias::new("target_from"),
        )
        .expr_as(Expr::cust("COUNT(*)"), Alias::new("claimable_count"))
        .from_as(readings::Entity, r.clone())
        .and_where(Expr::cust("r.site_id = d.site_id"))
        .and_where(Expr::cust("r.parameter_id = d.parameter_id"))
        .and_where(Expr::col((r.clone(), readings::Column::DeploymentId)).is_null())
        .and_where(Expr::cust("r.measurement_type IS DISTINCT FROM 'derived'"))
        .and_where(Expr::cust("r.time < d.deployed_from"))
        .and_where(Expr::cust(
            "(pe.prior_end IS NULL OR r.time >= pe.prior_end)",
        ))
        .take();

    let mut open = Condition::all()
        .add(Expr::col((d.clone(), deployments::Column::DeployedUntil)).is_null())
        .add(Expr::cust("c.claimable_count > 0"));
    let mut values: Vec<sea_orm::Value> = Vec::new();
    if let Some(predicate) = project_filter_sql(scope, "s.project_id", &mut values) {
        open = open.add(Expr::cust_with_values(predicate, values));
    }

    let on_true = || Condition::all().add(Expr::cust("true"));
    let (sql, values) = SeaQuery::select()
        .expr_as(
            Expr::col((d.clone(), deployments::Column::Id)),
            Alias::new("deployment_id"),
        )
        .columns([
            (d.clone(), deployments::Column::SensorId),
            (d.clone(), deployments::Column::SiteId),
            (d.clone(), deployments::Column::ParameterId),
            (d.clone(), deployments::Column::DeployedFrom),
        ])
        .columns([
            (c.clone(), Alias::new("target_from")),
            (c.clone(), Alias::new("claimable_count")),
        ])
        .from_as(deployments::Entity, d.clone())
        .join_as(
            JoinType::InnerJoin,
            sites::Entity,
            s_.clone(),
            Expr::col((s_.clone(), sites::Column::Id))
                .equals((d.clone(), deployments::Column::SiteId)),
        )
        // `ON TRUE` rather than `JoinType::CrossJoin`, which the builder still writes an `ON`
        // clause after; the two mean the same thing.
        .join_lateral(JoinType::InnerJoin, prior_end, pe.clone(), on_true())
        .join_lateral(JoinType::InnerJoin, claimable, c.clone(), on_true())
        .cond_where(open)
        .order_by_expr(Expr::cust("c.claimable_count"), Order::Desc)
        .take()
        .build(PostgresQueryBuilder);
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    rows.iter()
        .map(|r| -> AppResult<BackfillCandidate> {
            let row = BackfillCandidateRow::from_query_result(r, "")?;
            Ok(BackfillCandidate {
                deployment_id: row.deployment_id,
                sensor_id: row.sensor_id,
                site_id: row.site_id,
                parameter_id: row.parameter_id,
                deployed_from: row.deployed_from.with_timezone(&chrono::Utc),
                target_from: row.target_from.with_timezone(&chrono::Utc),
                claimable_count: row.claimable_count,
            })
        })
        .collect()
}

/// List open deployments with claimable pre-deployment history, rolled up per site. Requires
/// `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/actions/backfill_candidates",
    responses((status = 200, description = "Backfill candidates", body = BackfillCandidatesResponse)),
    tag = "actions"
)]
pub async fn backfill_candidates(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    _: DenyScoped,
) -> AppResult<Json<BackfillCandidatesResponse>> {
    let candidates = fetch_backfill_candidates(&app_state.db, &scope).await?;

    let mut by_site_map: HashMap<Uuid, (i64, i64)> = HashMap::new();
    let mut total_claimable = 0i64;
    for c in &candidates {
        let e = by_site_map.entry(c.site_id).or_insert((0, 0));
        e.0 += 1;
        e.1 += c.claimable_count;
        total_claimable += c.claimable_count;
    }
    let by_site = by_site_map
        .into_iter()
        .map(
            |(site_id, (deployments, claimable_count))| BackfillSiteSummary {
                site_id,
                deployments,
                claimable_count,
            },
        )
        .collect();

    Ok(Json(BackfillCandidatesResponse {
        total_candidates: candidates.len(),
        total_claimable,
        by_site,
        candidates,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct BackfillAttributionRequest {
    /// Backfill every candidate.
    #[serde(default)]
    pub all: bool,
    /// Restrict to candidates at this site.
    pub site_id: Option<Uuid>,
    /// Restrict to these specific deployments.
    #[serde(default)]
    pub deployment_ids: Vec<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BackfillAttributionResponse {
    pub job_id: Uuid,
    pub status: String,
    pub deployments_updated: usize,
    pub estimated_readings: i64,
}

/// Backdate the selected open deployments to their earliest claimable reading (bounded by any prior
/// deployment), then window-reprocess the affected slots so the previously-unattributed readings are
/// stamped with `sensor_id`/`deployment_id`/`calibration_id`. Runs as one tracked job. Requires
/// `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/backfill_attribution",
    request_body = BackfillAttributionRequest,
    responses(
        (status = 200, description = "Backfill triggered", body = BackfillAttributionResponse),
        (status = 403, description = "A named site or deployment is outside the caller's projects, or nothing was named"),
    ),
    tag = "actions"
)]
pub async fn backfill_attribution(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<BackfillAttributionRequest>,
) -> AppResult<Json<BackfillAttributionResponse>> {
    let db = &app_state.db;

    // `all` with no site and no deployments is the whole installation; a request that names nothing
    // at all selects nothing and keeps its 400 below.
    let named = payload.site_id.is_some() || !payload.deployment_ids.is_empty() || !payload.all;
    require_named_target(&scope, named, "site or deployments")?;
    if let Some(site_id) = payload.site_id {
        require_sites_in_scope(db, &scope, &[site_id]).await?;
    }
    if !payload.deployment_ids.is_empty() {
        let sites = deployment_sites(db, &payload.deployment_ids).await?;
        require_sites_in_scope(db, &scope, &sites).await?;
    }

    let all_candidates = fetch_backfill_candidates(db, &scope).await?;
    let dep_filter: HashSet<Uuid> = payload.deployment_ids.iter().copied().collect();
    let selected: Vec<BackfillCandidate> = all_candidates
        .into_iter()
        .filter(|c| {
            if !dep_filter.is_empty() {
                dep_filter.contains(&c.deployment_id)
            } else if let Some(site) = payload.site_id {
                c.site_id == site
            } else {
                payload.all
            }
        })
        .collect();

    if selected.is_empty() {
        return Err(AppError::BadRequest(
            "No matching backfill candidates (pass all=true, a site_id, or deployment_ids)"
                .to_string(),
        ));
    }

    // Backdate each selected deployment to its target_from (>= prior end, so no slot overlap), then
    // re-chain that sensor's deployed_until. Collect the distinct slots + sensors touched.
    let estimated_readings: i64 = selected.iter().map(|c| c.claimable_count).sum();
    let mut slots: HashSet<(Uuid, Uuid)> = HashSet::new();
    let mut sensors: HashSet<Uuid> = HashSet::new();
    for c in &selected {
        // Idempotent backdate: only move `deployed_from` earlier. After the first apply the row sits
        // at `target_from`, so a client retry replayed against another replica matches no rows
        // (`deployed_from > target_from` is false) and can't double-apply or re-widen the window.
        deployments::Entity::update_many()
            .col_expr(
                deployments::Column::DeployedFrom,
                Expr::value(c.target_from),
            )
            .filter(deployments::Column::Id.eq(c.deployment_id))
            .filter(deployments::Column::DeployedFrom.gt(c.target_from))
            .exec(db)
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
        slots.insert((c.site_id, c.parameter_id));
        sensors.insert(c.sensor_id);
    }
    for sensor_id in &sensors {
        recompute_deployed_until(db, *sensor_id)
            .await
            .map_err(|e| AppError::Internal(e.to_string()))?;
    }

    let deployments_updated = selected.len();
    let slots_param: Vec<[Uuid; 2]> = slots.into_iter().map(|(s, p)| [s, p]).collect();
    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        db,
        "backfill_attribution",
        None,
        None,
        &serde_json::json!({ "slots": slots_param }),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?
    .ok_or_else(|| AppError::Internal("failed to enqueue backfill_attribution job".to_string()))?;

    Ok(Json(BackfillAttributionResponse {
        job_id,
        status: "queued".to_string(),
        deployments_updated,
        estimated_readings,
    }))
}

// ---------------------------------------------------------------------------
// Calibration backfill
// ---------------------------------------------------------------------------

// A reading no curve covers is not an anomaly: an instrument nobody has calibrated yet, and a gap
// between two calibration campaigns, are ordinary states, and the readings in them are served raw.
// What these two queries surface is the narrower set of rows the stored state cannot explain.
//
// Both read the readings hypertable without an index that fits them. `calibration_id IS NULL` is
// not served by `idx_readings_calibration_id`, which is partial on `IS NOT NULL`, and the
// orphaned-correction predicate compares two columns of the same row, which no index can answer.
// Each therefore reads every reading in its window, and since the auto-minted identity curves were
// retired `calibration_id IS NULL` is the ordinary state of an uncorrected reading rather than a
// rarity, so the rows that survive the predicate are many. A time floor is what holds that cost
// still while the hypertable grows: it is the one bound TimescaleDB can turn into chunk exclusion,
// so the chunks below it are never opened at all.

/// How much of the readings history the anomaly report reads when the caller names no floor.
///
/// Measured back from the newest reading rather than from `now()`: an installation whose ingestion
/// has stalled would otherwise report on an empty window and read as clean.
const CANDIDATE_SCAN_DAYS: i64 = 90;

/// The default floor: [`CANDIDATE_SCAN_DAYS`] before the newest reading in the database, or `None`
/// when there are no readings, where a floor would make no difference.
///
/// The newest reading is taken across the whole table rather than the caller's projects, so two
/// callers reading the same report read the same window.
async fn default_scan_floor(
    db: &sea_orm::DatabaseConnection,
) -> AppResult<Option<chrono::DateTime<chrono::Utc>>> {
    // The stream ingest cursors carry the newest instant without touching the hypertable: an
    // unbounded MAX(time) over readings pays a planning cost proportional to the chunk count.
    // A batch-written reading newer than every cursor at most shifts the floor slightly later,
    // which the `since` parameter can always widen past. The unbounded probe remains only as
    // the fallback for a database with readings but no cursors at all.
    let mut newest: Option<chrono::DateTime<chrono::FixedOffset>> =
        crate::routes::private::data_streams::Entity::find()
            .select_only()
            .column_as(data_streams::Column::LastDataTime.max(), "newest")
            .into_tuple::<Option<chrono::DateTime<chrono::FixedOffset>>>()
            .one(db)
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
            .flatten();
    if newest.is_none() {
        newest = readings::Entity::find()
            .select_only()
            .column_as(readings::Column::Time.max(), "newest")
            .into_tuple::<Option<chrono::DateTime<chrono::FixedOffset>>>()
            .one(db)
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
            .flatten();
    }
    Ok(newest.map(|t| t.with_timezone(&chrono::Utc) - chrono::Duration::days(CANDIDATE_SCAN_DAYS)))
}

/// The caller's projects, reached through the sensor's deployments: an instrument deployed
/// nowhere resolves to no project and does not appear in a restricted caller's enumeration.
fn deployed_in_scope(scope: &AccessScope) -> Option<Expr> {
    let mut values: Vec<sea_orm::Value> = Vec::new();
    let predicate = project_filter_sql(scope, "s.project_id", &mut values)?;
    Some(Expr::cust_with_values(
        format!(
            "EXISTS (SELECT 1 FROM sensor_deployments d \
             JOIN sites s ON s.id = d.site_id \
             WHERE d.sensor_id = r.sensor_id AND {predicate})"
        ),
        values,
    ))
}

/// The caller's projects, reached through the reading's own site.
fn site_in_scope(scope: &AccessScope) -> Option<Expr> {
    let mut values: Vec<sea_orm::Value> = Vec::new();
    let predicate = project_filter_sql(scope, "s.project_id", &mut values)?;
    Some(Expr::cust_with_values(
        format!("EXISTS (SELECT 1 FROM sites s WHERE s.id = r.site_id AND {predicate})"),
        values,
    ))
}

/// The floor a scan reads from, or no condition at all for an unbounded scan.
fn scan_floor(since: Option<chrono::DateTime<chrono::Utc>>) -> Condition {
    match since {
        Some(t) => {
            Condition::all().add(Expr::col((Alias::new("r"), readings::Column::Time)).gte(t))
        }
        None => Condition::all(),
    }
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct CalibrationCandidatesQuery {
    /// Read only readings at or after this instant (ISO 8601). Defaults to 90 days before the
    /// newest reading; pass an earlier instant to widen the report, which costs a proportionally
    /// longer read of the hypertable. Whatever is used comes back as `scanned_from`.
    pub since: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize, ToSchema, Clone)]
pub struct CalibrationBackfillCandidate {
    pub sensor_id: Uuid,
    /// Readings whose time falls inside one of this sensor's calibration windows, yet which carry
    /// no `calibration_id`. A reprocess resolves every one of them.
    pub uncalibrated_count: i64,
    pub target_from: chrono::DateTime<chrono::Utc>,
    #[schema(required)]
    pub earliest_calibration_from: Option<chrono::DateTime<chrono::Utc>>,
}

/// Readings carrying a correction no curve accounts for: neither a calibration nor a standard
/// curve is named, yet `calibrated_value` differs from `raw_value`. Reported, never rewritten,
/// the stored number is somebody's measurement and this code cannot know how it was produced.
///
/// The reprocess engines hold the same rows back (`service::orphaned_correction_rows`, the shared
/// definition this query uses), so nothing an operator can trigger overwrites one either.
#[derive(Debug, Serialize, ToSchema, Clone)]
pub struct OrphanedCorrection {
    #[schema(required)]
    pub sensor_id: Option<Uuid>,
    #[schema(required)]
    pub site_id: Option<Uuid>,
    #[schema(required)]
    pub parameter_id: Option<Uuid>,
    pub count: i64,
    pub first_time: chrono::DateTime<chrono::Utc>,
    pub last_time: chrono::DateTime<chrono::Utc>,
}

/// Readings corrected by a curve their own instrument does not own, grouped by the pair.
#[derive(Debug, Serialize, ToSchema)]
pub struct ForeignCurveUse {
    /// The instrument the readings name.
    #[schema(required)]
    pub sensor_id: Option<Uuid>,
    /// The curve they are corrected by, which belongs to `curve_sensor_id`.
    pub standard_curve_id: Uuid,
    pub curve_sensor_id: Uuid,
    #[schema(required)]
    pub curve_name: Option<String>,
    #[schema(required)]
    pub site_id: Option<Uuid>,
    #[schema(required)]
    pub parameter_id: Option<Uuid>,
    pub count: i64,
    pub first_time: chrono::DateTime<chrono::Utc>,
    pub last_time: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CalibrationBackfillCandidatesResponse {
    /// Readings corrected by a curve their own instrument does not own. Report-only.
    pub foreign_curve_uses: Vec<ForeignCurveUse>,
    pub total_foreign_curve_uses: i64,
    pub candidates: Vec<CalibrationBackfillCandidate>,
    pub total_candidates: usize,
    pub total_uncalibrated: i64,
    pub orphaned_corrections: Vec<OrphanedCorrection>,
    pub total_orphaned_corrections: i64,
    /// The earliest reading this report read. Every count in it is over `[scanned_from, ∞)` alone,
    /// so it is a floor and not a total: an anomaly in older data is not absent, it was not looked
    /// at. Widen the window with `since`. `null` means the whole history was read, which is also
    /// what an empty database reports.
    #[schema(required)]
    pub scanned_from: Option<chrono::DateTime<chrono::Utc>>,
}

/// Sensors carrying readings a calibration window covers but whose `calibration_id` was never
/// stamped. Repairable: `backfill_calibrations` reprocesses them against the windows that already
/// exist, and nothing is created.
///
/// Confined to the caller's projects by the sensor's deployments, the same rule the `sensors` CRUD
/// read applies. A sensor never deployed anywhere resolves to no project and so does not appear in
/// a restricted caller's enumeration, while `backfill_calibrations` still accepts it by name: an
/// instrument in inventory is nobody's project, and the enumeration is what must not leak.
///
/// `since` bounds the read, and with it the counts: `None` reads the whole history, which is what
/// `backfill_calibrations` asks for because the sensors it selects have to be all of them.
async fn fetch_calibration_candidates(
    db: &sea_orm::DatabaseConnection,
    scope: &AccessScope,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<Vec<CalibrationBackfillCandidate>> {
    use sea_orm::{ConnectionTrait, Statement};

    let r = Alias::new("r");
    let cw = Alias::new("cw");
    let mut scanned = scan_floor(since)
        .add(Expr::col((r.clone(), readings::Column::SensorId)).is_not_null())
        .add(Expr::col((r.clone(), readings::Column::CalibrationId)).is_null())
        .add(Expr::col((cw.clone(), Alias::new("id"))).is_not_null())
        .add(Expr::cust(
            crate::routes::private::sensor_calibrations::service::window_resolved_rows("r"),
        ));
    if let Some(predicate) = deployed_in_scope(scope) {
        scanned = scanned.add(predicate);
    }
    // The lateral is the same window pick the reprocess engine runs, so `cw.id IS NOT NULL` means
    // exactly "a reprocess would stamp a curve here". Grabs resolve their curves by hand at entry
    // and are never windowed, hence `window_resolved_rows`. It runs once per row the scan keeps, so
    // the floor is what decides how often: every other predicate here is a filter, not a lookup.
    let pick = crate::routes::private::sensor_calibrations::resolver::pick_calibration_query(
        "r.sensor_id",
    );
    let (sql, values) = SeaQuery::select()
        .column((r.clone(), readings::Column::SensorId))
        .expr_as(Expr::cust("COUNT(*)"), Alias::new("uncalibrated_count"))
        .expr_as(
            Func::min(Expr::col((r.clone(), readings::Column::Time))),
            Alias::new("target_from"),
        )
        .from_as(readings::Entity, r.clone())
        .join_lateral(
            JoinType::LeftJoin,
            pick,
            cw.clone(),
            Condition::all().add(Expr::cust("true")),
        )
        .cond_where(scanned)
        .add_group_by([Expr::col((r.clone(), readings::Column::SensorId)).into()])
        .order_by_expr(Expr::cust("COUNT(*)"), Order::Desc)
        .take()
        .build(PostgresQueryBuilder);

    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    let mut candidates = Vec::with_capacity(rows.len());
    for row in &rows {
        let CandidateRow {
            sensor_id,
            uncalibrated_count,
            target_from,
        } = CandidateRow::from_query_result(row, "")?;

        let earliest_calibration_from = sensor_calibrations::Entity::find()
            .select_only()
            .column(sensor_calibrations::Column::ValidFrom)
            .filter(sensor_calibrations::Column::SensorId.eq(sensor_id))
            .order_by_asc(sensor_calibrations::Column::ValidFrom)
            .into_tuple::<chrono::DateTime<chrono::FixedOffset>>()
            .one(db)
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
            .map(|vf| vf.with_timezone(&chrono::Utc));

        candidates.push(CalibrationBackfillCandidate {
            sensor_id,
            uncalibrated_count,
            target_from: target_from.with_timezone(&chrono::Utc),
            earliest_calibration_from,
        });
    }

    Ok(candidates)
}

/// Readings holding a correction with no curve behind it. Grouped by `(sensor, site, parameter)`
/// so an operator can see where they came from.
///
/// Derived readings are excluded by definition: a computed quantity is not an instrument
/// measurement plus a correction, so it names no curve on purpose. Confined to the caller's
/// projects through the reading's own site, which also drops site-less rows from a restricted
/// caller's enumeration.
///
/// `since` bounds the read, and with it the counts. This half has no selective predicate at all,
/// so it reads its whole window whether or not anything is wrong; the floor is the only thing
/// keeping that off the rest of the history.
/// Readings whose standard curve belongs to another instrument. Read-only: which side is wrong is
/// a judgement (the reading was split, or the curve was), and the repair is the pin's own
/// `curves` choice.
async fn fetch_foreign_curve_uses(
    db: &sea_orm::DatabaseConnection,
    scope: &AccessScope,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<Vec<ForeignCurveUse>> {
    use sea_orm::{ConnectionTrait, Statement};

    let r = Alias::new("r");
    let sc = Alias::new("sc");
    let mut scanned = scan_floor(since).add(Expr::cust(
        crate::routes::private::sensor_calibrations::service::foreign_curve_rows("r", "sc"),
    ));
    if let Some(predicate) = site_in_scope(scope) {
        scanned = scanned.add(predicate);
    }
    let (sql, values) = SeaQuery::select()
        .column((r.clone(), readings::Column::SensorId))
        .expr_as(
            Expr::col((sc.clone(), standard_curves::Column::Id)),
            Alias::new("standard_curve_id"),
        )
        .expr_as(
            Expr::col((sc.clone(), standard_curves::Column::SensorId)),
            Alias::new("curve_sensor_id"),
        )
        .expr_as(
            Expr::col((sc.clone(), standard_curves::Column::Name)),
            Alias::new("curve_name"),
        )
        .column((r.clone(), readings::Column::SiteId))
        .column((r.clone(), readings::Column::ParameterId))
        .expr_as(Expr::cust("COUNT(*)"), Alias::new("n"))
        .expr_as(
            Func::min(Expr::col((r.clone(), readings::Column::Time))),
            Alias::new("first_time"),
        )
        .expr_as(
            Func::max(Expr::col((r.clone(), readings::Column::Time))),
            Alias::new("last_time"),
        )
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::InnerJoin,
            standard_curves::Entity,
            sc.clone(),
            Expr::col((sc.clone(), standard_curves::Column::Id))
                .equals((r.clone(), readings::Column::StandardCurveId)),
        )
        .cond_where(scanned)
        .add_group_by([
            Expr::col((r.clone(), readings::Column::SensorId)).into(),
            Expr::col((sc.clone(), standard_curves::Column::Id)).into(),
            Expr::col((sc.clone(), standard_curves::Column::SensorId)).into(),
            Expr::col((sc.clone(), standard_curves::Column::Name)).into(),
            Expr::col((r.clone(), readings::Column::SiteId)).into(),
            Expr::col((r.clone(), readings::Column::ParameterId)).into(),
        ])
        .order_by_expr(Expr::cust("COUNT(*)"), Order::Desc)
        .take()
        .build(PostgresQueryBuilder);

    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    rows.iter()
        .map(|r| {
            let row = ForeignCurveRow::from_query_result(r, "")?;
            Ok(ForeignCurveUse {
                sensor_id: row.sensor_id,
                standard_curve_id: row.standard_curve_id,
                curve_sensor_id: row.curve_sensor_id,
                curve_name: row.curve_name,
                site_id: row.site_id,
                parameter_id: row.parameter_id,
                count: row.n,
                first_time: row.first_time.with_timezone(&chrono::Utc),
                last_time: row.last_time.with_timezone(&chrono::Utc),
            })
        })
        .collect()
}

async fn fetch_orphaned_corrections(
    db: &sea_orm::DatabaseConnection,
    scope: &AccessScope,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<Vec<OrphanedCorrection>> {
    use sea_orm::{ConnectionTrait, Statement};

    let r = Alias::new("r");
    let mut scanned = scan_floor(since)
        .add(Expr::cust(
            crate::routes::private::sensor_calibrations::service::orphaned_correction_rows("r"),
        ))
        .add(Expr::cust("r.measurement_type IS DISTINCT FROM 'derived'"));
    if let Some(predicate) = site_in_scope(scope) {
        scanned = scanned.add(predicate);
    }
    let (sql, values) = SeaQuery::select()
        .column((r.clone(), readings::Column::SensorId))
        .column((r.clone(), readings::Column::SiteId))
        .column((r.clone(), readings::Column::ParameterId))
        .expr_as(Expr::cust("COUNT(*)"), Alias::new("orphan_count"))
        .expr_as(
            Func::min(Expr::col((r.clone(), readings::Column::Time))),
            Alias::new("first_time"),
        )
        .expr_as(
            Func::max(Expr::col((r.clone(), readings::Column::Time))),
            Alias::new("last_time"),
        )
        .from_as(readings::Entity, r.clone())
        .cond_where(scanned)
        .add_group_by([
            Expr::col((r.clone(), readings::Column::SensorId)).into(),
            Expr::col((r.clone(), readings::Column::SiteId)).into(),
            Expr::col((r.clone(), readings::Column::ParameterId)).into(),
        ])
        .order_by_expr(Expr::cust("COUNT(*)"), Order::Desc)
        .take()
        .build(PostgresQueryBuilder);

    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    rows.iter()
        .map(|r| {
            let row = OrphanedCorrectionRow::from_query_result(r, "")?;
            Ok(OrphanedCorrection {
                sensor_id: row.sensor_id,
                site_id: row.site_id,
                parameter_id: row.parameter_id,
                count: row.orphan_count,
                first_time: row.first_time.with_timezone(&chrono::Utc),
                last_time: row.last_time.with_timezone(&chrono::Utc),
            })
        })
        .collect()
}

/// List calibration anomalies: readings a window covers but that carry no `calibration_id`, and
/// readings carrying a correction no curve accounts for. Requires `read_metadata`.
///
/// Both halves read the readings hypertable with no index behind them, so the report covers the
/// most recent 90 days of data unless `since` names an earlier floor. It is a report on a window,
/// not a census: `scanned_from` carries the floor that was used and every count is a floor for that
/// window alone. `backfill_calibrations` is not bounded this way, so a widened report and the
/// backfill it feeds agree on which sensors are repairable.
#[utoipa::path(
    get,
    path = "/api/actions/calibration_candidates",
    params(CalibrationCandidatesQuery),
    responses((status = 200, description = "Calibration backfill candidates", body = CalibrationBackfillCandidatesResponse)),
    tag = "actions"
)]
pub async fn calibration_candidates(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    _: DenyScoped,
    Query(query): Query<CalibrationCandidatesQuery>,
) -> AppResult<Json<CalibrationBackfillCandidatesResponse>> {
    let scanned_from = match query.since {
        Some(since) => Some(since),
        None => default_scan_floor(&app_state.db).await?,
    };
    let candidates = fetch_calibration_candidates(&app_state.db, &scope, scanned_from).await?;
    let orphaned_corrections =
        fetch_orphaned_corrections(&app_state.db, &scope, scanned_from).await?;
    let foreign_curve_uses = fetch_foreign_curve_uses(&app_state.db, &scope, scanned_from).await?;
    let total_uncalibrated: i64 = candidates.iter().map(|c| c.uncalibrated_count).sum();
    let total_orphaned_corrections: i64 = orphaned_corrections.iter().map(|c| c.count).sum();
    let total_foreign_curve_uses: i64 = foreign_curve_uses.iter().map(|c| c.count).sum();
    Ok(Json(CalibrationBackfillCandidatesResponse {
        total_candidates: candidates.len(),
        total_uncalibrated,
        candidates,
        total_orphaned_corrections,
        orphaned_corrections,
        total_foreign_curve_uses,
        foreign_curve_uses,
        scanned_from,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct BackfillCalibrationsRequest {
    #[serde(default)]
    pub all: bool,
    pub sensor_id: Option<Uuid>,
    #[serde(default)]
    pub sensor_ids: Vec<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BackfillCalibrationsResponse {
    pub job_id: Uuid,
    pub status: String,
    pub sensors_updated: usize,
    pub estimated_readings: i64,
}

/// Reprocess the sensors whose readings a calibration window covers but whose `calibration_id` was
/// never stamped, so each row picks up the curve that already covers it. No calibration is created:
/// a reading no window covers stays uncorrected, which is what it is. Runs as one tracked job.
/// Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/backfill_calibrations",
    request_body = BackfillCalibrationsRequest,
    responses(
        (status = 200, description = "Calibration backfill triggered", body = BackfillCalibrationsResponse),
        (status = 403, description = "A named sensor is outside the caller's projects, or no sensor was named"),
        (status = 404, description = "No such sensor"),
    ),
    tag = "actions"
)]
pub async fn backfill_calibrations(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<BackfillCalibrationsRequest>,
) -> AppResult<Json<BackfillCalibrationsResponse>> {
    let db = &app_state.db;

    // Named instruments are confined one by one, including inventory (`Unowned::Allow`): a sensor
    // with no deployment belongs to no project, and refusing it would put a newly imported
    // instrument out of reach of every member. `all` names nothing, so a restricted caller may not
    // use it.
    let named: Vec<Uuid> = payload
        .sensor_id
        .into_iter()
        .chain(payload.sensor_ids.iter().copied())
        .collect();
    require_named_target(&scope, !named.is_empty() || !payload.all, "sensor")?;
    for sensor_id in &named {
        let target = project_of_sensor(db, *sensor_id).await?;
        confine_target(&scope, &target, Unowned::Allow, "sensor")?;
    }

    // With instruments named, each was confined just above and the selection below keeps only
    // those, so the candidate query runs unfiltered and inventory stays reachable. With nothing
    // named the caller is unrestricted (`require_named_target`), and the query is unfiltered too.
    //
    // Unbounded in time as well, unlike the report: this selects the work rather than describing
    // it, and a sensor whose only unstamped readings are older than the report's window still has
    // to be repaired. One operator action pays for the whole-history read; the reads the dashboard
    // makes on every page load do not.
    let all_candidates = fetch_calibration_candidates(db, &AccessScope::Unrestricted, None).await?;
    let id_filter: HashSet<Uuid> = payload.sensor_ids.iter().copied().collect();
    let selected: Vec<CalibrationBackfillCandidate> = all_candidates
        .into_iter()
        .filter(|c| {
            if !id_filter.is_empty() {
                id_filter.contains(&c.sensor_id)
            } else if let Some(sid) = payload.sensor_id {
                c.sensor_id == sid
            } else {
                payload.all
            }
        })
        .collect();

    if selected.is_empty() {
        return Err(AppError::BadRequest(
            "No matching calibration backfill candidates (pass all=true, a sensor_id, or sensor_ids)".to_string(),
        ));
    }

    let estimated_readings: i64 = selected.iter().map(|c| c.uncalibrated_count).sum();
    // The candidate query is the whole selection: every sensor in it has readings an existing
    // window covers, so the reprocess alone resolves them. Orphaned corrections are reported by
    // `calibration_candidates` and are deliberately not enqueued here, a value nobody can trace to
    // a curve is an operator's question, not something to overwrite.
    let sensor_ids_touched: Vec<Uuid> = selected.iter().map(|c| c.sensor_id).collect();

    let sensors_updated = sensor_ids_touched.len();
    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        db,
        "backfill_calibrations",
        None,
        None,
        &serde_json::json!({ "sensors": sensor_ids_touched }),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?
    .ok_or_else(|| AppError::Internal("failed to enqueue backfill_calibrations job".to_string()))?;

    Ok(Json(BackfillCalibrationsResponse {
        job_id,
        status: "queued".to_string(),
        sensors_updated,
        estimated_readings,
    }))
}

// ---------------------------------------------------------------------------
// Undeclared sd estimators

/// The row shapes the raw operator-action queries return, so each mapping is checked against the
/// SELECT that fills it rather than against a column name written twice.
#[derive(FromQueryResult)]
struct BackfillCandidateRow {
    deployment_id: Uuid,
    sensor_id: Uuid,
    site_id: Uuid,
    parameter_id: Uuid,
    deployed_from: chrono::DateTime<chrono::FixedOffset>,
    target_from: chrono::DateTime<chrono::FixedOffset>,
    claimable_count: i64,
}

#[derive(FromQueryResult)]
struct OrphanedCorrectionRow {
    sensor_id: Option<Uuid>,
    site_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
    orphan_count: i64,
    first_time: chrono::DateTime<chrono::FixedOffset>,
    last_time: chrono::DateTime<chrono::FixedOffset>,
}

#[derive(Debug, Serialize, ToSchema, sea_orm::FromQueryResult)]
pub struct UndeclaredEstimatorSlot {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub site_name: String,
    pub parameter_name: String,
    pub parameter_code: String,
    pub site_parameter_id: Uuid,
    /// Samples at this slot computed under no declaration, ie. `sd_estimator_source = 'default'`.
    pub undeclared_samples: i64,
    /// Whether any stream feeding the slot ships a precomputed sd column. A slot whose source
    /// states an sd is one whose convention is answerable from the evidence; one that does not is
    /// a choice about what this lab publishes.
    pub source_reports_sd: bool,
    /// Every stream feeding the slot, as `source_system/source_key`.
    pub streams: serde_json::Value,
    /// Open holds at this slot, and how many carry the population-divisor signature. That second
    /// number is the evidence for the decision; this report states it and rules on nothing.
    pub open_holds: i64,
    pub population_signature_holds: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UndeclaredEstimatorsResponse {
    pub total_slots: usize,
    pub total_undeclared_samples: i64,
    /// Open holds across these slots that the population divisor would explain. Every one of them
    /// is blocked from plain acknowledgement until its slot declares an estimator.
    pub total_population_signature_holds: i64,
    pub slots: Vec<UndeclaredEstimatorSlot>,
}

/// Slots serving replicate statistics under no declared sd estimator.
///
/// The sources stored both divisors over the years, row by row within one stream, so the
/// convention cannot be inferred and is declared per slot instead. Until a slot declares one, its
/// samples are computed with the sample divisor and stamped `default`, which is what this lists.
/// No write path can notice this shape on its own: every ingest is individually valid, and the gap
/// is in what nobody stated.
///
/// Read-only. Which divisor a slot publishes is a question about this lab's practice and the
/// source's, so nothing here decides one.
#[utoipa::path(
    get,
    path = "/api/actions/undeclared_sd_estimators",
    responses((status = 200, description = "Slots with no declared sd estimator", body = UndeclaredEstimatorsResponse)),
    tag = "actions"
)]
pub async fn undeclared_sd_estimators(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<UndeclaredEstimatorsResponse>> {
    use sea_orm::{FromQueryResult, Statement};

    let population_sd = &*crate::routes::private::sync::service::POPULATION_SD_SQL;
    let sp = Alias::new("sp");
    let st = Alias::new("st");
    let p = Alias::new("p");
    let u = Alias::new("u");
    let s_ = Alias::new("s");
    let h = Alias::new("h");
    let sm = Alias::new("sm");
    let ds = Alias::new("ds");
    let ds2 = Alias::new("ds2");
    let hold = Alias::new("h");

    let undeclared = SeaQuery::select()
        .expr_as(
            Expr::cust("COUNT(*)::bigint"),
            Alias::new("undeclared_samples"),
        )
        .from_as(samples::Entity, sm.clone())
        .and_where(Expr::cust("sm.site_id = sp.site_id"))
        .and_where(Expr::cust("sm.parameter_id = sp.parameter_id"))
        .and_where(Expr::col((sm.clone(), samples::Column::SdEstimatorSource)).eq("default"))
        .take();

    let sources = SeaQuery::select()
        .expr_as(
            Expr::cust("bool_or(ds.metadata #>> '{replicates,portal_sd_column}' IS NOT NULL)"),
            Alias::new("source_reports_sd"),
        )
        .expr_as(
            Expr::cust(
                "jsonb_agg(jsonb_build_object('stream_id', ds.id, 'source_system', \
                 ds.source_system, 'source_key', ds.source_key))",
            ),
            Alias::new("streams"),
        )
        .from_as(data_streams::Entity, ds.clone())
        .and_where(Expr::cust("ds.site_parameter_id = sp.id"))
        .take();

    let holds = SeaQuery::select()
        .expr_as(Expr::cust("COUNT(*)::bigint"), Alias::new("open_holds"))
        .expr_as(
            Expr::cust(format!("COUNT(*) FILTER (WHERE {population_sd})::bigint")),
            Alias::new("population_signature_holds"),
        )
        .from_as(Alias::new("replicate_audit_holds"), hold.clone())
        .join_as(
            JoinType::InnerJoin,
            data_streams::Entity,
            ds2.clone(),
            Expr::cust("ds2.id = h.stream_id"),
        )
        .and_where(Expr::cust("ds2.site_parameter_id = sp.id"))
        .and_where(Expr::cust(format!(
            "h.kind = '{}'",
            HoldKind::ReplicateStats.as_str()
        )))
        .and_where(Expr::cust(format!(
            "h.status IN {}",
            HoldStatus::sql_list(&HoldStatus::OPEN)
        )))
        .take();

    let mut undeclared_slots = Condition::all()
        .add(Expr::col((sp.clone(), site_parameters::Column::SdEstimator)).is_null());
    let mut values: Vec<sea_orm::Value> = Vec::new();
    if let Some(predicate) = project_filter_sql(&scope, "st.project_id", &mut values) {
        undeclared_slots = undeclared_slots.add(Expr::cust_with_values(predicate, values));
    }

    let on_true = || Condition::all().add(Expr::cust("true"));
    let (sql, values) = SeaQuery::select()
        .columns([
            (sp.clone(), site_parameters::Column::SiteId),
            (sp.clone(), site_parameters::Column::ParameterId),
        ])
        .expr_as(
            Expr::col((sp.clone(), site_parameters::Column::Id)),
            Alias::new("site_parameter_id"),
        )
        .expr_as(
            Expr::col((st.clone(), sites::Column::Name)),
            Alias::new("site_name"),
        )
        .expr_as(
            Expr::col((p.clone(), parameters::Column::Name)),
            Alias::new("parameter_name"),
        )
        .expr_as(
            Expr::col((p.clone(), parameters::Column::Code)),
            Alias::new("parameter_code"),
        )
        .column((u.clone(), Alias::new("undeclared_samples")))
        .expr_as(
            Expr::cust("COALESCE(s.source_reports_sd, false)"),
            Alias::new("source_reports_sd"),
        )
        .expr_as(
            Expr::cust("COALESCE(s.streams, '[]'::jsonb)"),
            Alias::new("streams"),
        )
        .expr_as(
            Expr::cust("COALESCE(h.open_holds, 0)"),
            Alias::new("open_holds"),
        )
        .expr_as(
            Expr::cust("COALESCE(h.population_signature_holds, 0)"),
            Alias::new("population_signature_holds"),
        )
        .from_as(site_parameters::Entity, sp.clone())
        .join_as(
            JoinType::InnerJoin,
            sites::Entity,
            st.clone(),
            Expr::col((st.clone(), sites::Column::Id))
                .equals((sp.clone(), site_parameters::Column::SiteId)),
        )
        .join_as(
            JoinType::InnerJoin,
            parameters::Entity,
            p.clone(),
            Expr::col((p.clone(), parameters::Column::Id))
                .equals((sp.clone(), site_parameters::Column::ParameterId)),
        )
        .join_lateral(
            JoinType::InnerJoin,
            undeclared,
            u.clone(),
            Condition::all().add(Expr::cust("u.undeclared_samples > 0")),
        )
        .join_lateral(JoinType::LeftJoin, sources, s_.clone(), on_true())
        .join_lateral(JoinType::LeftJoin, holds, h.clone(), on_true())
        .cond_where(undeclared_slots)
        .order_by_expr(
            Expr::cust("COALESCE(h.population_signature_holds, 0)"),
            Order::Desc,
        )
        .order_by_expr(Expr::cust("u.undeclared_samples"), Order::Desc)
        .take()
        .build(PostgresQueryBuilder);

    let slots = UndeclaredEstimatorSlot::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(&app_state.db)
    .await?;

    Ok(Json(UndeclaredEstimatorsResponse {
        total_slots: slots.len(),
        total_undeclared_samples: slots.iter().map(|s| s.undeclared_samples).sum(),
        total_population_signature_holds: slots.iter().map(|s| s.population_signature_holds).sum(),
        slots,
    }))
}

/// One reading whose curation columns are not the fold of its live decisions.
#[derive(Debug, Serialize, ToSchema, sea_orm::FromQueryResult)]
pub struct CurationDriftRow {
    pub stream_id: Uuid,
    pub time: chrono::DateTime<chrono::FixedOffset>,
    pub replicate_index: i16,
    #[schema(required)]
    pub site_id: Option<Uuid>,
    #[schema(required)]
    pub parameter_id: Option<Uuid>,
    /// The curation columns the reading holds.
    #[schema(value_type = Object)]
    pub stored: serde_json::Value,
    /// What the reading's live decisions fold to. A column absent from it is one no decision
    /// asserts, which is not a disagreement.
    #[schema(value_type = Object)]
    pub folded: serde_json::Value,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CurationDriftResponse {
    /// Every disagreeing reading, not only the ones listed below.
    pub total: i64,
    /// The first `limit` of them, newest first, so a person can open one.
    pub rows: Vec<CurationDriftRow>,
}

/// Readings whose curation columns disagree with the decisions recorded against them.
///
/// The columns are the projection of the record, written by the same trigger in the writer's
/// transaction, so a disagreement means something wrote a column without recording the decision,
/// or a decision failed to project. Read-only: which side is wrong is itself a decision, a
/// rollback or a fresh decision, so nothing here picks one.
#[utoipa::path(
    get,
    path = "/api/actions/curation_drift",
    params(("limit" = Option<u32>, Query, description = "How many rows to list, default 50, max 500")),
    responses((status = 200, description = "Readings that disagree with their decision record", body = CurationDriftResponse)),
    tag = "actions"
)]
pub async fn curation_drift(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(params): Query<CurationDriftQuery>,
) -> AppResult<Json<CurationDriftResponse>> {
    use sea_orm::{ConnectionTrait, FromQueryResult, Statement};

    let limit = params.limit.unwrap_or(50).clamp(1, 500);
    let mut values: Vec<sea_orm::Value> = Vec::new();
    // A reading outside the token's projects is not this caller's to see, and an unpaired one
    // belongs to no project at all, so a scoped caller is shown neither.
    let project_filter = project_filter_sql(&scope, "st.project_id", &mut values)
        .map(|predicate| format!("WHERE {predicate}"))
        .unwrap_or_default();
    values.push(i64::from(limit).into());
    let limit_param = values.len();

    // Still spelled: the drift definition it reads from is `inconsistent_rows_sql`, a CTE with
    // nested laterals over a VALUES list, and a subquery in `FROM` takes a built statement or
    // nothing. It converts when that fragment does (C223).
    let sql = format!(
        r"SELECT d.stream_id, d.time, d.replicate_index, r.site_id, r.parameter_id,
                 jsonb_strip_nulls(jsonb_build_object(
                     'is_flagged', d.is_flagged, 'flag_reason', d.flag_reason,
                     'withdrawn_at', d.withdrawn_at, 'withdrawn_reason', d.withdrawn_reason,
                     'unverified', d.unverified, 'standard_curve_id', d.standard_curve_id,
                     'calibration_id', d.calibration_id, 'sensor_id', d.sensor_id,
                     'raw_value', d.raw_value)) AS stored,
                 COALESCE(d.folded, '{{}}'::jsonb) AS folded
          FROM ({drift}) d
          JOIN readings r ON r.stream_id = d.stream_id AND r.time = d.time
                         AND r.replicate_index = d.replicate_index
          LEFT JOIN sites st ON st.id = r.site_id
          {project_filter}
          ORDER BY d.time DESC
          LIMIT ${limit_param}",
        drift = crate::routes::private::readings::service::inconsistent_rows_sql(),
    );

    let rows = app_state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .iter()
        .map(|row| CurationDriftRow::from_query_result(row, ""))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Json(CurationDriftResponse {
        total: crate::routes::private::readings::service::curation_drift_count(&app_state.db)
            .await?,
        rows,
    }))
}

/// How many drift rows to list beside the count.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct CurationDriftQuery {
    pub limit: Option<u32>,
}
