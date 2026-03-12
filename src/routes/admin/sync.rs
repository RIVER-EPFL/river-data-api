use axum::{Json, Router, extract::State, routing::get};
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
