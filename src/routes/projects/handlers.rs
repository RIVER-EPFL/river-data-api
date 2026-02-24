use axum::{
    extract::{Path, State},
    Json,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

use crate::common::AppState;
use crate::entity::{projects, sites};
use crate::error::AppResult;
use crate::routes::resolve_project;
use crate::routes::sites::SiteResponse;

use super::types::ProjectResponse;

/// List all projects
#[utoipa::path(
    get,
    path = "/api/projects",
    responses(
        (status = 200, description = "Projects retrieved successfully", body = Vec<ProjectResponse>),
    ),
    tag = "projects"
)]
pub async fn list_projects(State(state): State<AppState>) -> AppResult<Json<Vec<ProjectResponse>>> {
    let projects_list = projects::Entity::find()
        .order_by_asc(projects::Column::Name)
        .all(&state.db)
        .await?;

    let response: Vec<ProjectResponse> = projects_list
        .into_iter()
        .map(|p| ProjectResponse {
            id: p.id,
            name: p.name,
            description: p.description,
        })
        .collect();

    Ok(Json(response))
}

/// Get a specific project by ID or name
#[utoipa::path(
    get,
    path = "/api/projects/{project_id}",
    params(
        ("project_id" = String, Path, description = "Project UUID or name"),
    ),
    responses(
        (status = 200, description = "Project retrieved successfully", body = ProjectResponse),
        (status = 404, description = "Project not found"),
    ),
    tag = "projects"
)]
pub async fn get_project(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> AppResult<Json<ProjectResponse>> {
    let project = resolve_project(&state.db, &project_id).await?;

    Ok(Json(ProjectResponse {
        id: project.id,
        name: project.name,
        description: project.description,
    }))
}

/// List sites belonging to a project
#[utoipa::path(
    get,
    path = "/api/projects/{project_id}/sites",
    params(
        ("project_id" = String, Path, description = "Project UUID or name"),
    ),
    responses(
        (status = 200, description = "Sites retrieved successfully", body = Vec<SiteResponse>),
        (status = 404, description = "Project not found"),
    ),
    tag = "projects"
)]
pub async fn list_project_sites(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> AppResult<Json<Vec<SiteResponse>>> {
    let project = resolve_project(&state.db, &project_id).await?;

    let sites_list = sites::Entity::find()
        .filter(sites::Column::ProjectId.eq(project.id))
        .order_by_asc(sites::Column::Name)
        .all(&state.db)
        .await?;

    let response: Vec<SiteResponse> = sites_list
        .into_iter()
        .map(|s| SiteResponse {
            id: s.id,
            project_id: s.project_id,
            name: s.name,
            latitude: s.latitude,
            longitude: s.longitude,
            altitude_m: s.altitude_m,
        })
        .collect();

    Ok(Json(response))
}
