use axum::{Json, extract::State};
use serde::Deserialize;
use uuid::Uuid;

use crate::common::AppState;
use crate::services::sync_state as state;
use crate::error::AppResult;
use crate::services::calibration::recalculate_derived_at_timestamp;

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
