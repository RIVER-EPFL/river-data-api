use axum::{Json, Router, extract::State, routing::get, routing::post};
use sea_orm::{EntityTrait, QueryOrder};
use serde::Serialize;

use crate::common::AppState;
use crate::entity::sync_state;
use crate::error::AppResult;

#[derive(Serialize)]
pub struct SyncStateResponse {
    pub site_parameter_id: uuid::Uuid,
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
        .order_by_asc(sync_state::Column::SiteParameterId)
        .all(&state.db)
        .await?;

    let response: Vec<SyncStateResponse> = states
        .into_iter()
        .map(|s| SyncStateResponse {
            site_parameter_id: s.site_parameter_id,
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

async fn trigger_sync(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let Some(ref vaisala_client) = state.vaisala_client else {
        return Err(crate::error::AppError::ServiceUnavailable(
            "Vaisala sync not configured".to_string(),
        ));
    };
    let vaisala_client = vaisala_client.clone();
    let db = state.db.clone();
    let max_history_days = state
        .config
        .vaisala
        .as_ref()
        .map(|v| v.max_history_days)
        .unwrap_or(90);
    tokio::spawn(async move {
        tracing::info!("Manual sync triggered via admin API");
        if let Err(e) = crate::connectors::vaisala::sync::sync_locations(&db, &vaisala_client).await
        {
            tracing::error!(error = %e, "Manual location sync failed");
        }
        if let Err(e) = crate::connectors::vaisala::sync::sync_readings(
            &db,
            &vaisala_client,
            max_history_days,
            true,
        )
        .await
        {
            tracing::error!(error = %e, "Manual readings sync failed");
        }
    });

    Ok(Json(serde_json::json!({ "status": "sync_triggered" })))
}
