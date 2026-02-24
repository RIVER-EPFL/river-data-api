use axum::{extract::State, routing::get, routing::post, Json, Router};
use sea_orm::{EntityTrait, QueryOrder};
use serde::Serialize;

use crate::common::AppState;
use crate::entity::sync_state;
use crate::error::AppResult;

#[derive(Serialize)]
pub struct SyncStateResponse {
    pub parameter_id: uuid::Uuid,
    pub last_data_time: Option<String>,
    pub last_sync_attempt: Option<String>,
    pub sync_status: Option<String>,
    pub error_message: Option<String>,
    pub retry_count: Option<i32>,
    pub last_full_sync: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/state", get(list_sync_states))
        .route("/trigger", post(trigger_sync))
}

async fn list_sync_states(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<SyncStateResponse>>> {
    let states = sync_state::Entity::find()
        .order_by_asc(sync_state::Column::ParameterId)
        .all(&state.db)
        .await?;

    let response: Vec<SyncStateResponse> = states
        .into_iter()
        .map(|s| SyncStateResponse {
            parameter_id: s.parameter_id,
            last_data_time: s.last_data_time.map(|t| t.to_rfc3339()),
            last_sync_attempt: s.last_sync_attempt.map(|t| t.to_rfc3339()),
            sync_status: s.sync_status,
            error_message: s.error_message,
            retry_count: s.retry_count,
            last_full_sync: s.last_full_sync.map(|t| t.to_rfc3339()),
        })
        .collect();

    Ok(Json(response))
}

async fn trigger_sync(
    State(state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
    // Fire-and-forget: trigger a full sync in the background
    let state_clone = state.clone();
    tokio::spawn(async move {
        tracing::info!("Manual sync triggered via admin API");
        if let Err(e) = crate::sync::worker::sync_locations(&state_clone.db, &state_clone.vaisala_client).await {
            tracing::error!(error = %e, "Manual location sync failed");
        }
        if let Err(e) = crate::sync::worker::sync_readings(
            &state_clone.db,
            &state_clone.vaisala_client,
            state_clone.config.vaisala_max_history_days,
            true, // force full sync
        ).await {
            tracing::error!(error = %e, "Manual readings sync failed");
        }
    });

    Ok(Json(serde_json::json!({ "status": "sync_triggered" })))
}
