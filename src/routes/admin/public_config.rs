use axum::{
    Json,
    extract::{Path, State},
};

use crate::common::AppState;
use crate::error::AppResult;
use crate::services::public_api_config::invalidate_config;

pub async fn invalidate_public_config(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    invalidate_config(&state.public_config_cache, &slug).await;
    Ok(Json(
        serde_json::json!({ "status": "invalidated", "slug": slug }),
    ))
}
