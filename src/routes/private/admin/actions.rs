use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::sensor_calibrations::services::{evaluate_formula, recalculate_derived_at_timestamp};
use crate::common::sync_state as state;

#[derive(Debug, Deserialize, ToSchema)]
pub struct RefreshAggregatesRequest {
    /// If true, refresh ALL continuous aggregates (slow). If false, incremental refresh.
    #[serde(default)]
    pub full: bool,
}

/// Fire-and-forget refresh of TimescaleDB continuous aggregates. Returns immediately;
/// the refresh runs in a tokio task with a 10-minute timeout. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/actions/refresh_aggregates",
    request_body = RefreshAggregatesRequest,
    responses(
        (status = 200, description = "Refresh triggered (status field is always 'triggered')"),
    ),
    tag = "actions"
)]
pub async fn refresh_aggregates(
    State(app_state): State<AppState>,
    Json(payload): Json<RefreshAggregatesRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let db = app_state.db.clone();
    let full = payload.full;

    tokio::spawn(async move {
        match tokio::time::timeout(std::time::Duration::from_secs(600), async {
            if full {
                tracing::info!("Triggered full aggregate refresh via service API");
                state::refresh_continuous_aggregates_full(&db).await;
            } else {
                tracing::info!("Triggered incremental aggregate refresh via service API");
                state::refresh_continuous_aggregates(&db, None).await;
            }
        }).await {
            Ok(()) => {}
            Err(_) => tracing::error!("Aggregate refresh task timed out after 10 minutes"),
        }
    });

    Ok(Json(serde_json::json!({ "status": "triggered" })))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ComputeDerivedRequest {
    pub site_timestamps: Vec<SiteTimestamps>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SiteTimestamps {
    pub site_id: Uuid,
    pub timestamps: Vec<chrono::DateTime<chrono::Utc>>,
}

/// Compute and upsert derived parameter values for the given (site, timestamp) pairs.
/// Runs derived formula evaluation against source readings. Triggers an aggregate refresh
/// on success. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/actions/compute_derived",
    request_body = ComputeDerivedRequest,
    responses(
        (status = 200, description = "Computation triggered; returns counts when complete"),
    ),
    tag = "actions"
)]
pub async fn compute_derived(
    State(app_state): State<AppState>,
    Json(payload): Json<ComputeDerivedRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let db = app_state.db.clone();

    let total_timestamps: usize = payload
        .site_timestamps
        .iter()
        .map(|st| st.timestamps.len())
        .sum();

    tokio::spawn(async move {
        match tokio::time::timeout(std::time::Duration::from_secs(600), async {
            tracing::info!(
                sites = payload.site_timestamps.len(),
                timestamps = total_timestamps,
                "Computing derived values via service API"
            );

            let mut computed = 0u64;
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
        }).await {
            Ok(()) => {}
            Err(_) => tracing::error!("Compute derived task timed out after 10 minutes"),
        }
    });

    Ok(Json(
        serde_json::json!({ "status": "triggered", "total_timestamps": total_timestamps }),
    ))
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
            r"SELECT id, sensor_id, site_id, deployed_from FROM sensor_deployments WHERE id = $1",
            [payload.deployment_id.into()],
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
        .ok_or_else(|| AppError::NotFound("Deployment not found".into()))?;

    let sensor_id: Uuid = target.try_get("", "sensor_id").map_err(|e| AppError::Internal(format!("{e}")))?;
    let target_deployed_from: chrono::DateTime<chrono::FixedOffset> =
        target.try_get("", "deployed_from").map_err(|e| AppError::Internal(format!("{e}")))?;

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
    let previous_site_id: Option<Uuid> = previous.as_ref().and_then(|r| r.try_get("", "site_id").ok());

    // 3. Re-open the previous deployment
    if let Some(prev_id) = previous_deployment_id {
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE sensor_deployments SET deployed_until = NULL WHERE id = $1",
            [prev_id.into()],
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
    }

    // 4. Reassign readings from the rolled-back deployment
    let reassign_result = if let (Some(prev_id), Some(prev_site)) = (previous_deployment_id, previous_site_id) {
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings SET deployment_id = $1, site_id = $2 WHERE deployment_id = $3",
            [prev_id.into(), prev_site.into(), payload.deployment_id.into()],
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
    } else {
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings SET deployment_id = NULL WHERE deployment_id = $1",
            [payload.deployment_id.into()],
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
    };

    let readings_reassigned = reassign_result.rows_affected();

    // 5. Delete the rolled-back deployment
    db.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"DELETE FROM sensor_deployments WHERE id = $1",
        [payload.deployment_id.into()],
    ))
    .await
    .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    tracing::info!(
        deployment_id = %payload.deployment_id,
        sensor_id = %sensor_id,
        readings_reassigned,
        previous = ?previous_deployment_id,
        "Rolled back deployment"
    );

    // 6. Background aggregate refresh
    let db_clone = db.clone();
    tokio::spawn(async move {
        state::refresh_continuous_aggregates(&db_clone, None).await;
    });

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
    Json(payload): Json<PreviewDerivedRequest>,
) -> AppResult<Json<PreviewDerivedResponse>> {
    use sea_orm::{ConnectionTrait, Statement};

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
