use axum::{
    Json,
    extract::{Path, State},
};

use crate::common::AppState;
use crate::error::AppResult;
use crate::routes::public::service::invalidate_config;

/// Invalidate the in-memory cache for a public project's API config. Use after editing
/// public visibility settings to force a re-read on next public API request. Requires
/// `write_metadata`.
#[utoipa::path(
    post,
    path = "/actions/invalidate_public_config/{code}",
    params(("code" = String, Path, description = "Public project code")),
    responses(
        (status = 200, description = "Cache invalidated"),
    ),
    tag = "actions"
)]
pub async fn invalidate_public_config(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    invalidate_config(&state.public_config_cache, &code).await;
    Ok(Json(
        serde_json::json!({ "status": "invalidated", "code": code }),
    ))
}
