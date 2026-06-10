use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::{DenyScoped, ProjectScope, enforce_project_scope_for_sites};
use crate::error::{AppError, AppResult};
use crate::routes::private::sensor_calibrations::services::{
    evaluate_formula, recalculate_derived_at_timestamp, recompute_deployed_until,
    recompute_valid_until, reprocess_site_parameter_readings, reprocess_sensor_readings,
    spawn_tracked_job,
};
use crate::common::sync_state as state;

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
    let full = payload.full;
    let trigger_type = if full {
        "refresh_aggregates_full"
    } else {
        "refresh_aggregates"
    };

    let job_id = spawn_tracked_job(
        &app_state.db,
        None,
        trigger_type,
        None,
        app_state.events.clone(),
        move |db| async move {
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(600), async {
                if full {
                    tracing::info!("Triggered full aggregate refresh via service API");
                    state::refresh_continuous_aggregates_full(&db).await;
                } else {
                    tracing::info!("Triggered incremental aggregate refresh via service API");
                    state::refresh_continuous_aggregates(&db, None).await;
                }
            })
            .await;
            match outcome {
                Ok(()) => Ok(0),
                Err(_) => Err(sea_orm::DbErr::Custom(
                    "Aggregate refresh task timed out after 10 minutes".to_string(),
                )),
            }
        },
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(serde_json::json!({ "job_id": job_id, "status": "pending" })))
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
    ),
    tag = "actions"
)]
pub async fn compute_derived(
    State(app_state): State<AppState>,
    Json(payload): Json<ComputeDerivedRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let total_timestamps: usize = payload
        .site_timestamps
        .iter()
        .map(|st| st.timestamps.len())
        .sum();

    let job_id = spawn_tracked_job(
        &app_state.db,
        None,
        "compute_derived",
        None,
        app_state.events.clone(),
        move |db| {
            let payload = payload.clone();
            async move {
            tracing::info!(
                sites = payload.site_timestamps.len(),
                timestamps = total_timestamps,
                "Computing derived values via service API"
            );

            let mut computed = 0i64;
            for st in &payload.site_timestamps {
                for time in &st.timestamps {
                    match recalculate_derived_at_timestamp(&db, st.site_id, *time).await {
                        Ok(()) => computed += 1,
                        Err(e) => tracing::warn!(
                            error = %e,
                            site_id = %st.site_id,
                            time = %time,
                            "Failed to compute derived values"
                        ),
                    }
                }
            }

            tracing::info!(computed, "Derived computation complete");

            if computed > 0 {
                let min_time = payload
                    .site_timestamps
                    .iter()
                    .flat_map(|st| st.timestamps.iter())
                    .min()
                    .copied();
                if let Some(since) = min_time {
                    tracing::info!(%since, "Refreshing continuous aggregates after derived computation");
                    state::refresh_continuous_aggregates(&db, Some(since)).await;
                }
            }

            Ok(computed)
            }
        },
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(serde_json::json!({
        "job_id": job_id,
        "status": "pending",
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
    ),
    tag = "actions"
)]
pub async fn reprocess_sensor(
    State(app_state): State<AppState>,
    Json(payload): Json<ReprocessSensorRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let sensor_id = payload.sensor_id;
    let job_id = spawn_tracked_job(
        &app_state.db,
        Some(sensor_id),
        "manual_reprocess",
        None,
        app_state.events.clone(),
        move |db| async move { reprocess_sensor_readings(&db, sensor_id).await.map(|c| c as i64) },
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(serde_json::json!({ "job_id": job_id, "status": "pending" })))
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
    ),
    tag = "actions"
)]
pub async fn reprocess_all(
    State(app_state): State<AppState>,
) -> AppResult<Json<ReprocessAllResponse>> {
    use sea_orm::{ConnectionTrait, Statement};

    let db = &app_state.db;
    let slot_rows = db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT DISTINCT site_id, parameter_id FROM sensor_deployments".to_owned(),
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
    let slots: Vec<(Uuid, Uuid)> = slot_rows
        .into_iter()
        .filter_map(|r| {
            let s: Uuid = r.try_get("", "site_id").ok()?;
            let p: Uuid = r.try_get("", "parameter_id").ok()?;
            Some((s, p))
        })
        .collect();
    let slot_count = slots.len();

    let job_id = spawn_tracked_job(
        db,
        None,
        "reprocess_all",
        None,
        app_state.events.clone(),
        move |db| {
            let slots = slots.clone();
            async move {
                let mut total = 0i64;
                for (site_id, parameter_id) in slots {
                    match reprocess_site_parameter_readings(&db, site_id, parameter_id).await {
                        Ok(n) => total += n as i64,
                        Err(e) => tracing::warn!(
                            error = %e,
                            site_id = %site_id,
                            parameter_id = %parameter_id,
                            "reprocess_all: slot reprocess failed"
                        ),
                    }
                }
                tracing::info!(readings_updated = total, "reprocess_all complete");
                Ok(total)
            }
        },
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(ReprocessAllResponse {
        job_id,
        status: "pending".to_string(),
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
    ),
    tag = "actions"
)]
pub async fn rebuild_alarm_events(
    State(app_state): State<AppState>,
    Json(payload): Json<RebuildAlarmEventsRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let RebuildAlarmEventsRequest {
        site_id,
        parameter_id,
        start,
        end,
    } = payload;

    let job_id = spawn_tracked_job(
        &app_state.db,
        None,
        "alarm_backfill",
        None,
        app_state.events.clone(),
        move |db| async move {
            crate::routes::private::alarms::episodes::rebuild_alarm_events(
                &db,
                site_id,
                parameter_id,
                start,
                end,
            )
            .await
        },
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(serde_json::json!({ "job_id": job_id, "status": "pending" })))
}

/// Force a full open-alarm reconcile right now, instead of waiting for the periodic backstop
/// sweep. Runs the same single tick the sweeper runs (open new breaches, refresh still-breaching,
/// auto-resolve returned-to-range) across every active slot, synchronously — the post-LATERAL
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
        let _ = app_state.events.send(crate::common::AppEvent::AlarmStateChanged {
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
        (status = 404, description = "Deployment not found"),
    ),
    tag = "actions"
)]
pub async fn rollback_deployment(
    State(app_state): State<AppState>,
    Json(payload): Json<RollbackDeploymentRequest>,
) -> AppResult<Json<RollbackDeploymentResponse>> {
    use sea_orm::{ConnectionTrait, Statement};

    let db = &app_state.db;

    // 1. Load the target deployment
    let target = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id, sensor_id, site_id, deployed_from, deployed_until
              FROM sensor_deployments WHERE id = $1",
            [payload.deployment_id.into()],
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
        .ok_or_else(|| AppError::NotFound("Deployment not found".into()))?;

    let sensor_id: Uuid = target.try_get("", "sensor_id").map_err(|e| AppError::Internal(format!("{e}")))?;
    let target_deployed_from: chrono::DateTime<chrono::FixedOffset> =
        target.try_get("", "deployed_from").map_err(|e| AppError::Internal(format!("{e}")))?;
    // The boundary the rolled-back deployment vacates — the previous deployment re-extends to here
    // (NULL = the target was open-ended, so the previous reopens open-ended too).
    let target_deployed_until: Option<chrono::DateTime<chrono::FixedOffset>> =
        target.try_get("", "deployed_until").map_err(|e| AppError::Internal(format!("{e}")))?;

    // 2. Find the previous deployment for the same sensor
    let previous = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT id, site_id FROM sensor_deployments
              WHERE sensor_id = $1 AND deployed_from < $2 AND id != $3
              ORDER BY deployed_from DESC LIMIT 1",
            [sensor_id.into(), target_deployed_from.into(), payload.deployment_id.into()],
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    let previous_deployment_id: Option<Uuid> = previous.as_ref().and_then(|r| r.try_get("", "id").ok());

    // 3. Clear readings' FK to the rolled-back deployment (readings.deployment_id has no ON DELETE
    //    action, so the row can't be removed while referenced), then delete the deployment.
    let cleared = db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings SET deployment_id = NULL WHERE deployment_id = $1",
            [payload.deployment_id.into()],
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
    let readings_reassigned = cleared.rows_affected();

    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"DELETE FROM sensor_deployments WHERE id = $1",
        [payload.deployment_id.into()],
    ))
    .await
    .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    // 4. Reopen the previous deployment to absorb the vacated window. `recompute_deployed_until` only
    //    ever SHORTENS (LEAST), so without this the previous deployment — which was auto-closed when
    //    the rolled-back one was created — stays closed and the readings would un-attribute (site_id
    //    NULL) instead of reverting to the previous site. Reopening to the target's own
    //    `deployed_until` reclaims exactly the window the target held (NULL = open-ended), which can't
    //    overlap anything the slot constraint already excluded.
    if let Some(prev_id) = previous_deployment_id {
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE sensor_deployments SET deployed_until = $1 WHERE id = $2",
            [target_deployed_until.into(), prev_id.into()],
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
    }

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

/// Math builtins recognized by meval — not treated as variable names
const MATH_BUILTINS: &[&str] = &[
    "sqrt", "abs", "ln", "log", "exp", "sin", "cos", "tan", "asin", "acos", "atan",
    "sinh", "cosh", "tanh", "floor", "ceil", "round", "signum",
    "min", "max", "pi", "e",
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

    // A project-scoped key may only preview a derived computation against a site in its project.
    enforce_project_scope_for_sites(&app_state.db, scope, &[payload.site_id]).await?;

    // Validate formula
    if payload.formula.len() > 1000 {
        return Err(AppError::BadRequest("Formula too long (max 1000 characters)".to_string()));
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
                r"SELECT time, COALESCE(calibrated_value, raw_value) as val
                  FROM readings
                  WHERE parameter_id = $1 AND site_id = $2 AND time >= $3 AND time <= $4
                  ORDER BY time ASC",
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
    let mut time_set: Vec<i64> = all_times.iter().map(chrono::DateTime::timestamp_millis).collect();
    time_set.sort_unstable();
    time_set.dedup();

    let times: Vec<chrono::DateTime<chrono::Utc>> = time_set
        .iter()
        .map(|ms| {
            chrono::DateTime::from_timestamp_millis(*ms).unwrap_or_default()
        })
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
    /// Earliest claimable unattributed reading — the date `deployed_from` would move back to.
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
async fn fetch_backfill_candidates(
    db: &sea_orm::DatabaseConnection,
) -> AppResult<Vec<BackfillCandidate>> {
    use sea_orm::{ConnectionTrait, Statement};
    let rows = db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT d.id AS deployment_id, d.sensor_id, d.site_id, d.parameter_id,
                     d.deployed_from, c.target_from, c.claimable_count
              FROM sensor_deployments d
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
              ORDER BY c.claimable_count DESC"
                .to_owned(),
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
    _: DenyScoped,
) -> AppResult<Json<BackfillCandidatesResponse>> {
    let candidates = fetch_backfill_candidates(&app_state.db).await?;

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
        .map(|(site_id, (deployments, claimable_count))| BackfillSiteSummary {
            site_id,
            deployments,
            claimable_count,
        })
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
    responses((status = 200, description = "Backfill triggered", body = BackfillAttributionResponse)),
    tag = "actions"
)]
pub async fn backfill_attribution(
    State(app_state): State<AppState>,
    Json(payload): Json<BackfillAttributionRequest>,
) -> AppResult<Json<BackfillAttributionResponse>> {
    use sea_orm::{ConnectionTrait, Statement};
    let db = &app_state.db;

    let all_candidates = fetch_backfill_candidates(db).await?;
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
            "No matching backfill candidates (pass all=true, a site_id, or deployment_ids)".to_string(),
        ));
    }

    // Backdate each selected deployment to its target_from (>= prior end, so no slot overlap), then
    // re-chain that sensor's deployed_until. Collect the distinct slots + sensors touched.
    let estimated_readings: i64 = selected.iter().map(|c| c.claimable_count).sum();
    let mut slots: HashSet<(Uuid, Uuid)> = HashSet::new();
    let mut sensors: HashSet<Uuid> = HashSet::new();
    for c in &selected {
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE sensor_deployments SET deployed_from = $1 WHERE id = $2",
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
    let slot_vec: Vec<(Uuid, Uuid)> = slots.into_iter().collect();
    let job_id = spawn_tracked_job(
        db,
        None,
        "backfill_attribution",
        None,
        app_state.events.clone(),
        move |db| {
            let slots = slot_vec.clone();
            async move {
                let mut total = 0i64;
                for (site_id, parameter_id) in slots {
                    match reprocess_site_parameter_readings(&db, site_id, parameter_id).await {
                        Ok(n) => total += n as i64,
                        Err(e) => tracing::warn!(
                            error = %e, site_id = %site_id, parameter_id = %parameter_id,
                            "backfill_attribution: slot reprocess failed"
                        ),
                    }
                }
                tracing::info!(readings_updated = total, "backfill_attribution complete");
                Ok(total)
            }
        },
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(BackfillAttributionResponse {
        job_id,
        status: "pending".to_string(),
        deployments_updated,
        estimated_readings,
    }))
}

// ---------------------------------------------------------------------------
// Calibration backfill
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema, Clone)]
pub struct CalibrationBackfillCandidate {
    pub sensor_id: Uuid,
    pub uncalibrated_count: i64,
    pub target_from: chrono::DateTime<chrono::Utc>,
    pub earliest_calibration_from: Option<chrono::DateTime<chrono::Utc>>,
    pub is_identity: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CalibrationBackfillCandidatesResponse {
    pub candidates: Vec<CalibrationBackfillCandidate>,
    pub total_candidates: usize,
    pub total_uncalibrated: i64,
}

async fn fetch_calibration_candidates(
    db: &sea_orm::DatabaseConnection,
) -> AppResult<Vec<CalibrationBackfillCandidate>> {
    use sea_orm::{ConnectionTrait, Statement};

    let rows = db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT r.sensor_id, COUNT(*) AS uncalibrated_count, MIN(r.time) AS target_from
              FROM readings r
              WHERE r.sensor_id IS NOT NULL AND r.calibration_id IS NULL
              GROUP BY r.sensor_id
              HAVING COUNT(*) > 0
              ORDER BY COUNT(*) DESC"
                .to_owned(),
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    let mut candidates = Vec::with_capacity(rows.len());
    for row in &rows {
        let sensor_id: Uuid = row.try_get("", "sensor_id")?;
        let uncalibrated_count: i64 = row.try_get("", "uncalibrated_count")?;
        let target_from: chrono::DateTime<chrono::FixedOffset> =
            row.try_get("", "target_from")?;

        let cal_row = db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT valid_from, slope, intercept
                  FROM sensor_calibrations
                  WHERE sensor_id = $1
                  ORDER BY valid_from ASC
                  LIMIT 1",
                [sensor_id.into()],
            ))
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

        let (earliest_calibration_from, is_identity) = if let Some(cr) = cal_row {
            let vf: chrono::DateTime<chrono::FixedOffset> = cr.try_get("", "valid_from")?;
            let slope: f64 = cr.try_get("", "slope")?;
            let intercept: f64 = cr.try_get("", "intercept")?;
            (
                Some(vf.with_timezone(&chrono::Utc)),
                (slope - 1.0).abs() < f64::EPSILON && intercept.abs() < f64::EPSILON,
            )
        } else {
            (None, false)
        };

        candidates.push(CalibrationBackfillCandidate {
            sensor_id,
            uncalibrated_count,
            target_from: target_from.with_timezone(&chrono::Utc),
            earliest_calibration_from,
            is_identity,
        });
    }

    Ok(candidates)
}

/// List sensors with uncalibrated readings. Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/actions/calibration_candidates",
    responses((status = 200, description = "Calibration backfill candidates", body = CalibrationBackfillCandidatesResponse)),
    tag = "actions"
)]
pub async fn calibration_candidates(
    State(app_state): State<AppState>,
    _: DenyScoped,
) -> AppResult<Json<CalibrationBackfillCandidatesResponse>> {
    let candidates = fetch_calibration_candidates(&app_state.db).await?;
    let total_uncalibrated: i64 = candidates.iter().map(|c| c.uncalibrated_count).sum();
    Ok(Json(CalibrationBackfillCandidatesResponse {
        total_candidates: candidates.len(),
        total_uncalibrated,
        candidates,
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

/// Ensure calibration coverage for sensors with uncalibrated readings, then reprocess. Creates an
/// identity calibration (slope=1, intercept=0) for gaps before the first real calibration, or
/// backdates an existing identity calibration. Runs as one tracked job. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/actions/backfill_calibrations",
    request_body = BackfillCalibrationsRequest,
    responses((status = 200, description = "Calibration backfill triggered", body = BackfillCalibrationsResponse)),
    tag = "actions"
)]
pub async fn backfill_calibrations(
    State(app_state): State<AppState>,
    Json(payload): Json<BackfillCalibrationsRequest>,
) -> AppResult<Json<BackfillCalibrationsResponse>> {
    use sea_orm::{ConnectionTrait, Statement};
    let db = &app_state.db;

    let all_candidates = fetch_calibration_candidates(db).await?;
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
    let mut sensor_ids_touched: Vec<Uuid> = Vec::new();

    for c in &selected {
        match (c.earliest_calibration_from, c.is_identity) {
            // No calibrations at all — create identity starting at earliest reading
            (None, _) => {
                let cal_id = Uuid::new_v4();
                db.execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    r"INSERT INTO sensor_calibrations
                          (id, sensor_id, slope, intercept, valid_from, performed_by, notes, created_at)
                      VALUES ($1, $2, 1.0, 0.0, $3, 'system', 'Identity calibration (backfill)', NOW())",
                    [cal_id.into(), c.sensor_id.into(), c.target_from.into()],
                ))
                .await
                .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
            }
            // Earliest calibration is identity — backdate it
            (Some(_), true) => {
                db.execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    r"UPDATE sensor_calibrations
                      SET valid_from = $1
                      WHERE sensor_id = $2
                        AND slope = 1.0 AND intercept = 0.0
                        AND valid_from = (
                            SELECT MIN(valid_from) FROM sensor_calibrations WHERE sensor_id = $2
                        )",
                    [c.target_from.into(), c.sensor_id.into()],
                ))
                .await
                .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
            }
            // Earliest calibration is real — insert identity before it
            (Some(_), false) => {
                let cal_id = Uuid::new_v4();
                db.execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    r"INSERT INTO sensor_calibrations
                          (id, sensor_id, slope, intercept, valid_from, performed_by, notes, created_at)
                      VALUES ($1, $2, 1.0, 0.0, $3, 'system', 'Identity calibration (backfill)', NOW())",
                    [cal_id.into(), c.sensor_id.into(), c.target_from.into()],
                ))
                .await
                .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
            }
        }

        recompute_valid_until(db, c.sensor_id)
            .await
            .map_err(|e| AppError::Internal(e.to_string()))?;
        sensor_ids_touched.push(c.sensor_id);
    }

    let sensors_updated = sensor_ids_touched.len();
    let job_id = spawn_tracked_job(
        db,
        None,
        "backfill_calibrations",
        None,
        app_state.events.clone(),
        move |db| {
            let sensors = sensor_ids_touched.clone();
            async move {
                let mut total = 0i64;
                for sensor_id in sensors {
                    match reprocess_sensor_readings(&db, sensor_id).await {
                        Ok(n) => total += n as i64,
                        Err(e) => tracing::warn!(
                            error = %e, sensor_id = %sensor_id,
                            "backfill_calibrations: sensor reprocess failed"
                        ),
                    }
                }
                tracing::info!(readings_updated = total, "backfill_calibrations complete");
                Ok(total)
            }
        },
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(BackfillCalibrationsResponse {
        job_id,
        status: "pending".to_string(),
        sensors_updated,
        estimated_readings,
    }))
}
