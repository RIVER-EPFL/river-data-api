use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::services::calibration::{evaluate_formula, recalculate_derived_at_timestamp};
use crate::services::sync_state as state;

// ============================================================================
// Refresh Aggregates
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct RefreshAggregatesRequest {
    #[serde(default)]
    pub full: bool,
}

pub async fn refresh_aggregates(
    State(app_state): State<AppState>,
    Json(payload): Json<RefreshAggregatesRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let db = app_state.db.clone();
    let full = payload.full;

    tokio::spawn(async move {
        if full {
            tracing::info!("Triggered full aggregate refresh via service API");
            state::refresh_continuous_aggregates_full(&db).await;
        } else {
            tracing::info!("Triggered incremental aggregate refresh via service API");
            state::refresh_continuous_aggregates(&db).await;
        }
    });

    Ok(Json(serde_json::json!({ "status": "triggered" })))
}

// ============================================================================
// Update Last Full Sync
// ============================================================================

pub async fn update_last_full_sync(
    State(app_state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
    tracing::info!("Updating last_full_sync for all parameters via service API");
    state::update_last_full_sync_for_all_parameters(&app_state.db).await;
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

// ============================================================================
// Compute Derived
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ComputeDerivedRequest {
    pub site_timestamps: Vec<SiteTimestamps>,
}

#[derive(Debug, Deserialize)]
pub struct SiteTimestamps {
    pub site_id: Uuid,
    pub timestamps: Vec<chrono::DateTime<chrono::Utc>>,
}

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
    });

    Ok(Json(
        serde_json::json!({ "status": "triggered", "total_timestamps": total_timestamps }),
    ))
}

// ============================================================================
// Preview Derived Formula
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct PreviewDerivedRequest {
    pub formula: String,
    pub site_id: Uuid,
    pub start: chrono::DateTime<chrono::Utc>,
    pub end: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
pub struct PreviewDerivedResponse {
    pub site: PreviewSite,
    pub times: Vec<chrono::DateTime<chrono::Utc>>,
    pub source_parameters: Vec<SourceParameterSeries>,
    pub derived: DerivedSeries,
}

#[derive(Debug, Serialize)]
pub struct PreviewSite {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct SourceParameterSeries {
    pub name: String,
    pub units: String,
    pub values: Vec<Option<f64>>,
}

#[derive(Debug, Serialize)]
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
        .map(|s| s.to_string())
        .collect()
}

pub async fn preview_derived(
    State(app_state): State<AppState>,
    Json(payload): Json<PreviewDerivedRequest>,
) -> AppResult<Json<PreviewDerivedResponse>> {
    use sea_orm::{ConnectionTrait, Statement};

    // Validate formula
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
    // variable name = parameter type name → parameters.name → site_parameters.parameter_type_id
    let mut param_info: Vec<(String, Uuid, Uuid, String)> = Vec::new(); // (var_name, site_param_id, parameter_id, units)

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
        // If not found at this site, skip — we'll produce nulls for those timestamps
    }

    // Fetch readings for all resolved parameters within time range
    // Build a time → variable → value map
    let mut all_times: Vec<chrono::DateTime<chrono::Utc>> = Vec::new();
    let mut source_data: HashMap<String, HashMap<i64, f64>> = HashMap::new(); // var_name → (timestamp_ms → value)
    let mut source_units: HashMap<String, String> = HashMap::new();

    for (var_name, _sp_id, parameter_id, units) in &param_info {
        source_units.insert(var_name.clone(), units.clone());

        let rows = db
            .query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT time, COALESCE(calibrated_value, raw_value) as val
                  FROM readings
                  WHERE parameter_id = $1 AND time >= $2 AND time <= $3
                  ORDER BY time ASC",
                [
                    (*parameter_id).into(),
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
    let mut time_set: Vec<i64> = all_times.iter().map(|t| t.timestamp_millis()).collect();
    time_set.sort();
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
        // Build variable map for this timestamp
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
