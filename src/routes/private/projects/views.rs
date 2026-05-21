use axum::{
    Json,
    extract::{Path, State},
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::entity::sites;
use crate::error::{AppError, AppResult};
use crate::routes::resolve_project;
use crate::routes::private::sites::types::SiteResponse;

/// List sites belonging to a project
#[utoipa::path(
    get,
    path = "/{project_id}/sites",
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
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<Vec<SiteResponse>>> {
    let project = resolve_project(&state.db, &project_id).await?;

    // Enforce project scope
    if let Some(scope_project) = scope
        && project.id != scope_project
    {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }

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
