use axum::{Json, extract::{Query, State}};
use sea_orm::{DatabaseBackend, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct SearchParams {
    /// Free-text query (minimum 2 characters). Matched case-insensitively against
    /// site name, sensor serial/name, parameter name/display_name, and project name.
    pub q: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SearchResponse {
    pub query: String,
    pub results: SearchResults,
    pub total: usize,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SearchResults {
    pub sites: Vec<SiteResult>,
    pub sensors: Vec<SensorResult>,
    pub parameters: Vec<ParameterResult>,
    pub projects: Vec<ProjectResult>,
}

#[derive(Debug, Serialize, FromQueryResult, ToSchema)]
pub struct SiteResult {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Serialize, FromQueryResult, ToSchema)]
pub struct SensorResult {
    pub id: Uuid,
    pub serial_number: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Serialize, FromQueryResult, ToSchema)]
pub struct ParameterResult {
    pub id: Uuid,
    pub name: String,
    pub display_name: String,
}

#[derive(Debug, Serialize, FromQueryResult, ToSchema)]
pub struct ProjectResult {
    pub id: Uuid,
    pub name: String,
}

/// Cross-entity full-text search. Matches against sites, sensors, parameters, and
/// projects by name (case-insensitive substring). Requires `read_metadata`.
#[utoipa::path(
    get,
    path = "/search",
    params(SearchParams),
    responses(
        (status = 200, description = "Matching entities grouped by type", body = SearchResponse),
        (status = 400, description = "Query shorter than 2 characters"),
    ),
    tag = "search"
)]
pub async fn search(
    State(state): State<AppState>,
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

    let (sites, sensors, parameters, projects) = tokio::try_join!(
        SiteResult::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT id, name FROM sites WHERE name ILIKE $1 ORDER BY name LIMIT 10",
            [pattern.clone().into()],
        ))
        .all(&state.db),
        SensorResult::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT id, serial_number, name FROM sensors WHERE serial_number ILIKE $1 OR name ILIKE $1 ORDER BY serial_number LIMIT 10",
            [pattern.clone().into()],
        ))
        .all(&state.db),
        ParameterResult::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT id, name, display_name FROM parameters WHERE name ILIKE $1 OR display_name ILIKE $1 ORDER BY name LIMIT 10",
            [pattern.clone().into()],
        ))
        .all(&state.db),
        ProjectResult::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT id, name FROM projects WHERE name ILIKE $1 ORDER BY name LIMIT 10",
            [pattern.clone().into()],
        ))
        .all(&state.db),
    )?;

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
