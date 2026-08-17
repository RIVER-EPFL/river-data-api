use axum::{
    Json,
    extract::{Query, State},
};
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
use crate::routes::private::sensors::calibrations::service::{
    evaluate_formula, recompute_deployed_until, reprocess_sensor_readings,
};
use crate::routes::private::sensors::deployments::slots;

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
    use sea_orm::{ConnectionTrait, Statement};
    if deployment_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<sea_orm::Value> = deployment_ids.iter().map(|id| (*id).into()).collect();
    let placeholders: Vec<String> = (1..=ids.len()).map(|n| format!("${n}")).collect();
    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT DISTINCT site_id FROM sensor_deployments WHERE id IN ({})",
                placeholders.join(",")
            ),
            ids,
        ))
        .await
        .map_err(AppError::Database)?;
    Ok(rows
        .iter()
        .filter_map(|r| r.try_get::<Uuid>("", "site_id").ok())
        .collect())
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
    /// If true, refresh ALL continuous aggregates (slow). If false, incremental refresh.
    #[serde(default)]
    pub full: bool,
}

/// Refresh of TimescaleDB continuous aggregates, tracked as a `reprocessing_jobs` row.
/// Returns immediately with the job id; the refresh runs in a background task with a
/// 10-minute timeout (a timeout marks the job `failed`). Requires `write_data`.
#[utoipa::path(
    post,
    path = "/actions/refresh_aggregates",
    request_body = RefreshAggregatesRequest,
    responses(
        (status = 200, description = "Refresh triggered; returns job_id and status 'pending'"),
    ),
    tag = "actions"
)]
pub async fn refresh_aggregates(
    State(app_state): State<AppState>,
    Json(payload): Json<RefreshAggregatesRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let trigger_type = if payload.full {
        "refresh_aggregates_full"
    } else {
        "refresh_aggregates"
    };

    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        &app_state.db,
        trigger_type,
        None,
        None,
        &serde_json::json!({ "full": payload.full }),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(
        serde_json::json!({ "job_id": job_id, "status": "queued" }),
    ))
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
    path = "/actions/compute_derived",
    request_body = ComputeDerivedRequest,
    responses(
        (status = 200, description = "Computation triggered; returns job_id, status 'pending', total_timestamps"),
        (status = 403, description = "A named site is outside the caller's projects, or no site was named"),
    ),
    tag = "actions"
)]
pub async fn compute_derived(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<ComputeDerivedRequest>,
) -> AppResult<Json<serde_json::Value>> {
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

    Ok(Json(serde_json::json!({
        "job_id": job_id,
        "status": "queued",
        "total_timestamps": total_timestamps,
    })))
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
    path = "/actions/reprocess",
    request_body = ReprocessSensorRequest,
    responses(
        (status = 200, description = "Reprocessing triggered; returns job_id and status 'pending'"),
        (status = 403, description = "The sensor is deployed only outside the caller's projects"),
        (status = 404, description = "No such sensor"),
    ),
    tag = "actions"
)]
pub async fn reprocess_sensor(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<ReprocessSensorRequest>,
) -> AppResult<Json<serde_json::Value>> {
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

    Ok(Json(
        serde_json::json!({ "job_id": job_id, "status": "queued" }),
    ))
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
    path = "/actions/reprocess_all",
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
    use sea_orm::{ConnectionTrait, Statement};

    // The backdate has no target field at all: it re-derives every slot in the installation.
    require_named_target(&scope, false, "sensor (POST /actions/reprocess)")?;

    let db = &app_state.db;
    let slot_rows = db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT DISTINCT site_id, parameter_id FROM sensor_deployments".to_owned(),
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
    // Count the slots only to report it back synchronously; the job re-reads `sensor_deployments`
    // itself, so a rerun reflects the current topology.
    let slot_count = slot_rows
        .into_iter()
        .filter(|r| {
            r.try_get::<Uuid>("", "site_id").is_ok()
                && r.try_get::<Uuid>("", "parameter_id").is_ok()
        })
        .count();

    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
        db,
        "reprocess_all",
        None,
        None,
        &serde_json::json!({}),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?
    .ok_or_else(|| AppError::Internal("failed to enqueue reprocess_all job".to_string()))?;

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
    path = "/actions/rebuild_alarm_events",
    request_body = RebuildAlarmEventsRequest,
    responses(
        (status = 200, description = "Rebuild triggered; returns job_id and status 'pending'"),
        (status = 403, description = "The named site is outside the caller's projects, or no site was named"),
    ),
    tag = "actions"
)]
pub async fn rebuild_alarm_events(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<RebuildAlarmEventsRequest>,
) -> AppResult<Json<serde_json::Value>> {
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

    Ok(Json(
        serde_json::json!({ "job_id": job_id, "status": "queued" }),
    ))
}

/// Force a full open-alarm reconcile right now, instead of waiting for the periodic backstop
/// sweep. Runs the same single tick the sweeper runs (open new breaches, refresh still-breaching,
/// auto-resolve returned-to-range) across every active slot, synchronously, the post-LATERAL
/// breach query is O(active slots), so this returns in well under a second. Operator escape hatch
/// for "I changed something and want the alarm state correct immediately". Requires `write_data`.
#[utoipa::path(
    post,
    path = "/actions/reconcile_alarms",
    responses(
        (status = 200, description = "Reconcile complete; counts of opened/updated/resolved events"),
    ),
    tag = "actions"
)]
pub async fn reconcile_alarms(
    State(app_state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
    let stats =
        crate::routes::private::alarms::sweeper::evaluate_alarm_events(&app_state.db).await?;

    if stats.opened > 0 || stats.resolved > 0 {
        let _ = app_state
            .events
            .send(crate::common::AppEvent::AlarmStateChanged {
                opened: stats.opened,
                resolved: stats.resolved,
            });
    }

    Ok(Json(serde_json::json!({
        "opened": stats.opened,
        "updated": stats.updated,
        "resolved": stats.resolved,
    })))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RollbackDeploymentRequest {
    pub deployment_id: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RollbackDeploymentResponse {
    pub status: String,
    pub readings_reassigned: u64,
    pub previous_deployment_id: Option<Uuid>,
}

/// Undo the most recent sensor deployment, reassigning its readings back to the previous
/// deployment's site. Used after an accidentally-created deployment. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/actions/rollback_deployment",
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
    use sea_orm::{ConnectionTrait, Statement, TransactionTrait};

    let db = &app_state.db;

    // 1. Load the target deployment
    let target = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id, sensor_id, site_id, parameter_id, deployed_from, deployed_until
              FROM sensor_deployments WHERE id = $1",
            [payload.deployment_id.into()],
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
        .ok_or_else(|| AppError::NotFound("Deployment not found".into()))?;

    // A deployment is always at a site, so its project is the site's. This deletes the row, the
    // same destruction `DELETE /sensor_deployments/{id}` performs under `enforce_scope_on_crud`.
    let deployment_site: Uuid = target
        .try_get("", "site_id")
        .map_err(|e| AppError::Internal(format!("{e}")))?;
    let owner = project_of_site(db, deployment_site).await?;
    confine_target(&scope, &owner, Unowned::Deny, "deployment")?;

    let sensor_id: Uuid = target
        .try_get("", "sensor_id")
        .map_err(|e| AppError::Internal(format!("{e}")))?;
    let parameter_id: Uuid = target
        .try_get("", "parameter_id")
        .map_err(|e| AppError::Internal(format!("{e}")))?;
    let target_deployed_from: chrono::DateTime<chrono::FixedOffset> = target
        .try_get("", "deployed_from")
        .map_err(|e| AppError::Internal(format!("{e}")))?;
    // The boundary the rolled-back deployment vacates, the previous deployment re-extends to here
    // (NULL = the target was open-ended, so the previous reopens open-ended too).
    let target_deployed_until: Option<chrono::DateTime<chrono::FixedOffset>> = target
        .try_get("", "deployed_until")
        .map_err(|e| AppError::Internal(format!("{e}")))?;

    // 2. Find the previous deployment for the same sensor AND THE SAME PARAMETER, on a multi-channel
    //    instrument the immediately-prior deployment by time could belong to a different channel;
    //    reopening that one would extend the wrong channel's window.
    let previous = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id, site_id, deployed_from FROM sensor_deployments
              WHERE sensor_id = $1 AND parameter_id = $4 AND deployed_from < $2 AND id != $3
              ORDER BY deployed_from DESC LIMIT 1",
            [
                sensor_id.into(),
                target_deployed_from.into(),
                payload.deployment_id.into(),
                parameter_id.into(),
            ],
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    let previous_deployment_id: Option<Uuid> =
        previous.as_ref().and_then(|r| r.try_get("", "id").ok());

    // 3-4. Clear readings' FK to the rolled-back deployment, delete it, and reopen the previous
    //    deployment, atomically, so a mid-operation failure can't leave the deployment deleted with
    //    nothing reopened (which would silently un-attribute its readings). The decompression cap is
    //    lifted so the readings FK-clear can't fail on old compressed chunks.
    let txn = db
        .begin()
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
    txn.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0".to_owned(),
    ))
    .await
    .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    let cleared = txn
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings SET deployment_id = NULL WHERE deployment_id = $1",
            [payload.deployment_id.into()],
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
    let readings_reassigned = cleared.rows_affected();

    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"DELETE FROM sensor_deployments WHERE id = $1",
        [payload.deployment_id.into()],
    ))
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
    if let Some(prev_id) = previous_deployment_id {
        let prev = previous.as_ref().expect("previous row present with its id");
        let prev_site: Uuid = prev
            .try_get("", "site_id")
            .map_err(|e| AppError::Internal(format!("{e}")))?;
        let prev_from: chrono::DateTime<chrono::FixedOffset> = prev
            .try_get("", "deployed_from")
            .map_err(|e| AppError::Internal(format!("{e}")))?;

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

        txn.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE sensor_deployments SET deployed_until = $1 WHERE id = $2",
            [target_deployed_until.into(), prev_id.into()],
        ))
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
    reprocess_sensor_readings(db, sensor_id)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

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

/// Math builtins recognized by meval, not treated as variable names
const MATH_BUILTINS: &[&str] = &[
    "sqrt", "abs", "ln", "log", "exp", "sin", "cos", "tan", "asin", "acos", "atan", "sinh", "cosh",
    "tanh", "floor", "ceil", "round", "signum", "min", "max", "pi", "e",
];

/// Extract variable names from a formula (identifiers that aren't math builtins)
fn extract_variables(formula: &str) -> Vec<String> {
    let builtins: HashSet<&str> = MATH_BUILTINS.iter().copied().collect();
    let mut tokens = Vec::new();
    let mut start = None;
    for (i, c) in formula.char_indices() {
        if c.is_alphanumeric() || c == '_' {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start {
            tokens.push(&formula[s..i]);
            start = None;
        }
    }
    if let Some(s) = start {
        tokens.push(&formula[s..]);
    }

    let mut seen = HashSet::new();
    tokens
        .into_iter()
        .filter(|t| {
            !t.chars().next().is_some_and(|c| c.is_ascii_digit())
                && !builtins.contains(t)
                && seen.insert(t.to_string())
        })
        .map(std::string::ToString::to_string)
        .collect()
}

/// Preview a derived parameter formula against historical source readings at a given site,
/// WITHOUT writing anything to the database. Used by the formula builder UI to validate
/// formulas before saving. Requires `read_data`.
#[utoipa::path(
    post,
    path = "/actions/preview_derived",
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
    let site_row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT name FROM sites WHERE id = $1",
            [payload.site_id.into()],
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
        .ok_or_else(|| AppError::NotFound("Site not found".into()))?;

    let site_name: String = site_row
        .try_get("", "name")
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    // Extract variable names from formula
    let var_names = extract_variables(&payload.formula);

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
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT sp.id as sp_id, sp.parameter_id, COALESCE(sp.display_units, '') as units
                  FROM site_parameters sp
                  JOIN parameters pt ON pt.id = sp.parameter_id
                  WHERE sp.site_id = $1 AND pt.name = $2
                  LIMIT 1",
                [payload.site_id.into(), var_name.clone().into()],
            ))
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

        if let Some(row) = row {
            let sp_id: Uuid = row
                .try_get("", "sp_id")
                .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
            let parameter_id: Uuid = row
                .try_get("", "parameter_id")
                .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
            let units: String = row
                .try_get("", "units")
                .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
            param_info.push((var_name.clone(), sp_id, parameter_id, units));
        }
    }

    // Fetch readings for all resolved parameters within time range
    let mut all_times: Vec<chrono::DateTime<chrono::Utc>> = Vec::new();
    let mut source_data: HashMap<String, HashMap<i64, f64>> = HashMap::new();
    let mut source_units: HashMap<String, String> = HashMap::new();

    for (var_name, _sp_id, parameter_id, units) in &param_info {
        source_units.insert(var_name.clone(), units.clone());

        let rows = db
            .query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT DISTINCT ON (r.time)
                         r.time, COALESCE(smp.mean, r.calibrated_value, r.raw_value) as val
                  FROM readings r
                  LEFT JOIN samples smp ON smp.id = r.sample_id
                  WHERE r.parameter_id = $1 AND r.site_id = $2 AND r.time >= $3 AND r.time <= $4
                    AND r.replicate_index = 0
                  ORDER BY r.time ASC, (r.measurement_type IS NOT DISTINCT FROM 'spot') ASC, r.stream_id",
                [
                    (*parameter_id).into(),
                    payload.site_id.into(),
                    payload.start.into(),
                    payload.end.into(),
                ],
            ))
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

        let map = source_data.entry(var_name.clone()).or_default();
        for row in rows {
            let time: chrono::DateTime<chrono::FixedOffset> = row
                .try_get("", "time")
                .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
            let val: f64 = row
                .try_get("", "val")
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
        let mut vars = HashMap::new();
        let mut all_present = true;

        for var_name in &var_names {
            if let Some(data) = source_data.get(var_name) {
                if let Some(&val) = data.get(ms) {
                    vars.insert(var_name.clone(), val);
                } else {
                    all_present = false;
                    break;
                }
            } else {
                all_present = false;
                break;
            }
        }

        if !all_present {
            derived_values.push(None);
            derived_errors.push(None);
            continue;
        }

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
    let mut values: Vec<sea_orm::Value> = Vec::new();
    let project_filter = project_filter_sql(scope, "s.project_id", &mut values)
        .map(|predicate| format!("AND {predicate}"))
        .unwrap_or_default();
    let sql = format!(
        r"SELECT d.id AS deployment_id, d.sensor_id, d.site_id, d.parameter_id,
                 d.deployed_from, c.target_from, c.claimable_count
          FROM sensor_deployments d
          JOIN sites s ON s.id = d.site_id
          CROSS JOIN LATERAL (
              SELECT MAX(p.deployed_until) AS prior_end
              FROM sensor_deployments p
              WHERE p.site_id = d.site_id AND p.parameter_id = d.parameter_id
                AND p.id <> d.id AND p.deployed_until IS NOT NULL
                AND p.deployed_until <= d.deployed_from
          ) pe
          CROSS JOIN LATERAL (
              SELECT MIN(r.time) AS target_from, COUNT(*) AS claimable_count
              FROM readings r
              WHERE r.site_id = d.site_id AND r.parameter_id = d.parameter_id
                AND r.sensor_id IS NULL AND r.time < d.deployed_from
                AND (pe.prior_end IS NULL OR r.time >= pe.prior_end)
          ) c
          WHERE d.deployed_until IS NULL AND c.claimable_count > 0
          {project_filter}
          ORDER BY c.claimable_count DESC"
    );
    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    rows.iter()
        .map(|r| -> AppResult<BackfillCandidate> {
            let deployed_from: chrono::DateTime<chrono::FixedOffset> =
                r.try_get("", "deployed_from")?;
            let target_from: chrono::DateTime<chrono::FixedOffset> =
                r.try_get("", "target_from")?;
            Ok(BackfillCandidate {
                deployment_id: r.try_get("", "deployment_id")?,
                sensor_id: r.try_get("", "sensor_id")?,
                site_id: r.try_get("", "site_id")?,
                parameter_id: r.try_get("", "parameter_id")?,
                deployed_from: deployed_from.with_timezone(&chrono::Utc),
                target_from: target_from.with_timezone(&chrono::Utc),
                claimable_count: r.try_get("", "claimable_count")?,
            })
        })
        .collect()
}

/// List open deployments with claimable pre-deployment history, rolled up per site. Requires
/// `read_metadata`.
#[utoipa::path(
    get,
    path = "/actions/backfill_candidates",
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
    path = "/actions/backfill_attribution",
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
    use sea_orm::{ConnectionTrait, Statement};
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
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE sensor_deployments SET deployed_from = $1 WHERE id = $2 AND deployed_from > $1",
            [c.target_from.into(), c.deployment_id.into()],
        ))
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
    use sea_orm::{ConnectionTrait, Statement};

    let row = db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT MAX(time) AS newest FROM readings".to_string(),
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    let newest: Option<chrono::DateTime<chrono::FixedOffset>> = match row {
        Some(r) => r.try_get("", "newest")?,
        None => None,
    };
    Ok(newest.map(|t| t.with_timezone(&chrono::Utc) - chrono::Duration::days(CANDIDATE_SCAN_DAYS)))
}

/// `AND r.time >= $n` for a floor, registering its value; empty for an unbounded scan.
fn scan_floor_sql(
    since: Option<chrono::DateTime<chrono::Utc>>,
    values: &mut Vec<sea_orm::Value>,
) -> String {
    match since {
        Some(t) => {
            values.push(t.into());
            format!("AND r.time >= ${}", values.len())
        }
        None => String::new(),
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
    pub sensor_id: Option<Uuid>,
    pub site_id: Option<Uuid>,
    pub parameter_id: Option<Uuid>,
    pub count: i64,
    pub first_time: chrono::DateTime<chrono::Utc>,
    pub last_time: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CalibrationBackfillCandidatesResponse {
    pub candidates: Vec<CalibrationBackfillCandidate>,
    pub total_candidates: usize,
    pub total_uncalibrated: i64,
    pub orphaned_corrections: Vec<OrphanedCorrection>,
    pub total_orphaned_corrections: i64,
    /// The earliest reading this report read. Every count in it is over `[scanned_from, ∞)` alone,
    /// so it is a floor and not a total: an anomaly in older data is not absent, it was not looked
    /// at. Widen the window with `since`. `null` means the whole history was read, which is also
    /// what an empty database reports.
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

    let mut values: Vec<sea_orm::Value> = Vec::new();
    let time_filter = scan_floor_sql(since, &mut values);
    let project_filter = project_filter_sql(scope, "s.project_id", &mut values)
        .map(|predicate| {
            format!(
                "AND EXISTS (SELECT 1 FROM sensor_deployments d \
                 JOIN sites s ON s.id = d.site_id \
                 WHERE d.sensor_id = r.sensor_id AND {predicate})"
            )
        })
        .unwrap_or_default();
    // The lateral is the same window pick the reprocess engine runs, so `cw.id IS NOT NULL` means
    // exactly "a reprocess would stamp a curve here". Grabs resolve their curves by hand at entry
    // and are never windowed, hence `window_resolved_rows`. It runs once per row the scan keeps, so
    // the floor is what decides how often: every other predicate here is a filter, not a lookup.
    let sql = format!(
        r"SELECT r.sensor_id, COUNT(*) AS uncalibrated_count, MIN(r.time) AS target_from
          FROM readings r
          LEFT JOIN LATERAL ({pick}) cw ON true
          WHERE r.sensor_id IS NOT NULL AND r.calibration_id IS NULL
            AND cw.id IS NOT NULL
            AND {windowed}
          {time_filter}
          {project_filter}
          GROUP BY r.sensor_id
          ORDER BY COUNT(*) DESC",
        pick = crate::routes::private::sensors::calibrations::resolver::pick_calibration_lateral(
            "r.sensor_id"
        ),
        windowed =
            crate::routes::private::sensors::calibrations::service::window_resolved_rows("r"),
    );

    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    let mut candidates = Vec::with_capacity(rows.len());
    for row in &rows {
        let sensor_id: Uuid = row.try_get("", "sensor_id")?;
        let uncalibrated_count: i64 = row.try_get("", "uncalibrated_count")?;
        let target_from: chrono::DateTime<chrono::FixedOffset> = row.try_get("", "target_from")?;

        let cal_row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT valid_from
                  FROM sensor_calibrations
                  WHERE sensor_id = $1
                  ORDER BY valid_from ASC
                  LIMIT 1",
                [sensor_id.into()],
            ))
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

        let earliest_calibration_from = match cal_row {
            Some(cr) => {
                let vf: chrono::DateTime<chrono::FixedOffset> = cr.try_get("", "valid_from")?;
                Some(vf.with_timezone(&chrono::Utc))
            }
            None => None,
        };

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
async fn fetch_orphaned_corrections(
    db: &sea_orm::DatabaseConnection,
    scope: &AccessScope,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<Vec<OrphanedCorrection>> {
    use sea_orm::{ConnectionTrait, Statement};

    let mut values: Vec<sea_orm::Value> = Vec::new();
    let time_filter = scan_floor_sql(since, &mut values);
    let project_filter = project_filter_sql(scope, "s.project_id", &mut values)
        .map(|predicate| {
            format!("AND EXISTS (SELECT 1 FROM sites s WHERE s.id = r.site_id AND {predicate})")
        })
        .unwrap_or_default();
    let sql = format!(
        r"SELECT r.sensor_id, r.site_id, r.parameter_id, COUNT(*) AS orphan_count,
                 MIN(r.time) AS first_time, MAX(r.time) AS last_time
          FROM readings r
          WHERE {orphaned}
            AND r.measurement_type IS DISTINCT FROM 'derived'
          {time_filter}
          {project_filter}
          GROUP BY r.sensor_id, r.site_id, r.parameter_id
          ORDER BY COUNT(*) DESC",
        orphaned =
            crate::routes::private::sensors::calibrations::service::orphaned_correction_rows("r"),
    );

    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    rows.iter()
        .map(|row| {
            let first: chrono::DateTime<chrono::FixedOffset> = row.try_get("", "first_time")?;
            let last: chrono::DateTime<chrono::FixedOffset> = row.try_get("", "last_time")?;
            Ok(OrphanedCorrection {
                sensor_id: row.try_get("", "sensor_id")?,
                site_id: row.try_get("", "site_id")?,
                parameter_id: row.try_get("", "parameter_id")?,
                count: row.try_get("", "orphan_count")?,
                first_time: first.with_timezone(&chrono::Utc),
                last_time: last.with_timezone(&chrono::Utc),
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
    path = "/actions/calibration_candidates",
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
    let total_uncalibrated: i64 = candidates.iter().map(|c| c.uncalibrated_count).sum();
    let total_orphaned_corrections: i64 = orphaned_corrections.iter().map(|c| c.count).sum();
    Ok(Json(CalibrationBackfillCandidatesResponse {
        total_candidates: candidates.len(),
        total_uncalibrated,
        candidates,
        total_orphaned_corrections,
        orphaned_corrections,
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
    path = "/actions/backfill_calibrations",
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
// Duplicate slots
// ---------------------------------------------------------------------------

// Nothing in the write path can see this shape. Deduplication is keyed on the readings primary key,
// `(stream_id, time, replicate_index)`, which is per channel, while the rollups group by
// `(site_id, parameter_id)`, which is per slot. Two channels paired to one slot therefore both
// ingest cleanly and both land in the same bucket, so an average over the period is taken over two
// populations at once.
//
// Read-only, and it stays that way. Which of two disagreeing copies is the measurement is a
// question about where each came from, which lives outside the database, so this reports the
// disagreement and its size and leaves the decision to an operator.

#[derive(Debug, Serialize, Deserialize, ToSchema, Clone)]
pub struct DuplicateSlotStream {
    pub stream_id: Uuid,
    pub source_system: String,
    pub source_key: String,
    /// Instants this stream contributes to the slot's overlap.
    pub instants: i64,
}

#[derive(Debug, Serialize, ToSchema, Clone)]
pub struct DuplicateSlot {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    /// Instants at this slot served by more than one stream.
    pub overlapping_instants: i64,
    /// Of those, the instants whose served values are not all equal. An instant where the copies
    /// agree is redundant rather than contradictory, and an average over it is still right.
    pub disagreeing_instants: i64,
    /// Largest and mean spread between the served values at one instant, over the disagreeing
    /// instants alone. A spread the size of the parameter's rounding step reads as one channel
    /// carrying fewer decimals than the other; a large one is two different measurements.
    pub max_difference: Option<f64>,
    pub mean_difference: Option<f64>,
    pub first_time: chrono::DateTime<chrono::Utc>,
    pub last_time: chrono::DateTime<chrono::Utc>,
    /// Every stream feeding the overlap, busiest first.
    pub streams: Vec<DuplicateSlotStream>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DuplicateSlotsResponse {
    pub total_slots: usize,
    pub total_overlapping_instants: i64,
    pub total_disagreeing_instants: i64,
    pub slots: Vec<DuplicateSlot>,
    /// The earliest reading this report read, on the same terms as
    /// `/actions/calibration_candidates`: every count is a floor for that window alone.
    pub scanned_from: Option<chrono::DateTime<chrono::Utc>>,
}

/// Slots where one instant is served by more than one stream, with the size of the disagreement.
///
/// The served value is `COALESCE(calibrated_value, raw_value)`, the number the rollups average, so
/// the spread reported here is the spread that reaches a chart. Flagged readings are excluded for
/// the same reason: the aggregates already leave them out, so a flagged copy is not a second
/// population. `replicate_index` is part of the instant, so a stream's own replicates are not an
/// overlap.
///
/// Confined to the caller's projects through the reading's own site.
async fn fetch_duplicate_slots(
    db: &sea_orm::DatabaseConnection,
    scope: &AccessScope,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<Vec<DuplicateSlot>> {
    use sea_orm::{ConnectionTrait, Statement};

    let mut values: Vec<sea_orm::Value> = Vec::new();
    let time_filter = scan_floor_sql(since, &mut values);
    let project_filter = project_filter_sql(scope, "s.project_id", &mut values)
        .map(|predicate| {
            format!("AND EXISTS (SELECT 1 FROM sites s WHERE s.id = r.site_id AND {predicate})")
        })
        .unwrap_or_default();
    // `overlap` is read twice, which is what keeps the hypertable read to one pass: Postgres
    // materialises a CTE with more than one reference rather than inlining it into both.
    let sql = format!(
        r"WITH overlap AS (
              SELECT r.site_id, r.parameter_id, r.time,
                     array_agg(DISTINCT r.stream_id) AS streams,
                     MAX(COALESCE(r.calibrated_value, r.raw_value))
                       - MIN(COALESCE(r.calibrated_value, r.raw_value)) AS spread
              FROM readings r
              WHERE r.site_id IS NOT NULL AND r.parameter_id IS NOT NULL
                AND r.is_flagged IS NOT TRUE
                {time_filter}
                {project_filter}
              GROUP BY r.site_id, r.parameter_id, r.time, r.replicate_index
              HAVING COUNT(DISTINCT r.stream_id) > 1
          ),
          per_stream AS (
              SELECT o.site_id, o.parameter_id, e AS stream_id, COUNT(*) AS instants
              FROM overlap o, unnest(o.streams) AS e
              GROUP BY o.site_id, o.parameter_id, e
          )
          SELECT a.site_id, a.parameter_id, a.overlapping_instants, a.disagreeing_instants,
                 a.max_difference, a.mean_difference, a.first_time, a.last_time,
                 COALESCE((
                     SELECT jsonb_agg(jsonb_build_object(
                                'stream_id', p.stream_id,
                                'source_system', d.source_system,
                                'source_key', d.source_key,
                                'instants', p.instants)
                            ORDER BY p.instants DESC, d.source_system)
                     FROM per_stream p
                     JOIN data_streams d ON d.id = p.stream_id
                     WHERE p.site_id = a.site_id AND p.parameter_id = a.parameter_id
                 ), '[]'::jsonb) AS streams
          FROM (
              SELECT site_id, parameter_id,
                     COUNT(*) AS overlapping_instants,
                     COUNT(*) FILTER (WHERE spread > 0) AS disagreeing_instants,
                     MAX(spread) FILTER (WHERE spread > 0) AS max_difference,
                     AVG(spread) FILTER (WHERE spread > 0) AS mean_difference,
                     MIN(time) AS first_time, MAX(time) AS last_time
              FROM overlap
              GROUP BY site_id, parameter_id
          ) a
          ORDER BY a.overlapping_instants DESC"
    );

    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    rows.iter()
        .map(|row| {
            let first: chrono::DateTime<chrono::FixedOffset> = row.try_get("", "first_time")?;
            let last: chrono::DateTime<chrono::FixedOffset> = row.try_get("", "last_time")?;
            let streams: serde_json::Value = row.try_get("", "streams")?;
            let streams: Vec<DuplicateSlotStream> = serde_json::from_value(streams)
                .map_err(|e| AppError::Internal(format!("malformed stream summary: {e}")))?;
            Ok(DuplicateSlot {
                site_id: row.try_get("", "site_id")?,
                parameter_id: row.try_get("", "parameter_id")?,
                overlapping_instants: row.try_get("", "overlapping_instants")?,
                disagreeing_instants: row.try_get("", "disagreeing_instants")?,
                max_difference: row.try_get("", "max_difference")?,
                mean_difference: row.try_get("", "mean_difference")?,
                first_time: first.with_timezone(&chrono::Utc),
                last_time: last.with_timezone(&chrono::Utc),
                streams,
            })
        })
        .collect()
}

/// List slots fed by more than one stream at the same instant. Requires `read_metadata`.
///
/// Reads the readings hypertable with no index behind it, so it covers the most recent 90 days
/// unless `since` names an earlier floor, and `scanned_from` reports which was used.
#[utoipa::path(
    get,
    path = "/actions/duplicate_slots",
    params(CalibrationCandidatesQuery),
    responses((status = 200, description = "Slots served by more than one stream", body = DuplicateSlotsResponse)),
    tag = "actions"
)]
pub async fn duplicate_slots(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    _: DenyScoped,
    Query(query): Query<CalibrationCandidatesQuery>,
) -> AppResult<Json<DuplicateSlotsResponse>> {
    let scanned_from = match query.since {
        Some(since) => Some(since),
        None => default_scan_floor(&app_state.db).await?,
    };
    let slots = fetch_duplicate_slots(&app_state.db, &scope, scanned_from).await?;
    Ok(Json(DuplicateSlotsResponse {
        total_slots: slots.len(),
        total_overlapping_instants: slots.iter().map(|s| s.overlapping_instants).sum(),
        total_disagreeing_instants: slots.iter().map(|s| s.disagreeing_instants).sum(),
        slots,
        scanned_from,
    }))
}
