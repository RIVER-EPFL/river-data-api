use axum::{
    Json,
    extract::{Path, State},
};

use crate::common::AppState;
use crate::error::AppResult;
use crate::routes::public::services::invalidate_config;

/// Invalidate the in-memory cache for a public project's API config. Use after editing
/// `public_exposed_parameters` to force a re-read on next public API request. Requires
/// `write_metadata`.
#[utoipa::path(
    post,
    path = "/actions/invalidate_public_config/{slug}",
    params(("slug" = String, Path, description = "Public project slug")),
    responses(
        (status = 200, description = "Cache invalidated"),
    ),
    tag = "actions"
)]
pub async fn invalidate_public_config(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    invalidate_config(&state.public_config_cache, &slug).await;
    Ok(Json(
        serde_json::json!({ "status": "invalidated", "slug": slug }),
    ))
}
