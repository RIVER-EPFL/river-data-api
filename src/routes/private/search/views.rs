//! Cross-entity full-text search. Matches against sites, sensors, parameters, and projects by
//! name (case-insensitive substring). Requires `read_metadata`.

use axum::{
    Json,
    extract::{Query, State},
};

use super::models::{SearchParams, SearchResponse, SearchResults};
use super::service::matches;
use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::error::{AppError, AppResult};

#[utoipa::path(
    get,
    path = "/api/search",
    params(SearchParams),
    responses(
        (status = 200, description = "Matching entities grouped by type", body = SearchResponse),
        (status = 400, description = "Query shorter than 2 characters"),
    ),
    tag = "search"
)]
pub async fn search(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(params): Query<SearchParams>,
) -> AppResult<Json<SearchResponse>> {
    let query = params.q.trim();

    if query.len() < 2 {
        return Err(AppError::BadRequest(
            "Search query must be at least 2 characters".to_string(),
        ));
    }

    if query.len() > 200 {
        return Err(AppError::BadRequest(
            "Search query too long (max 200 characters)".to_string(),
        ));
    }

    let pattern = format!("%{query}%");
    let (sites, sensors, parameters, projects) = matches(&state.db, &pattern, &scope).await?;
    let total = sites.len() + sensors.len() + parameters.len() + projects.len();

    Ok(Json(SearchResponse {
        query: query.to_string(),
        results: SearchResults {
            sites,
            sensors,
            parameters,
            projects,
        },
        total,
    }))
}
