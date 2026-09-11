use crate::routes::public::service::invalidate_config;
use axum::middleware;
use axum::routing::get;
use axum::{
    Json,
    extract::{Path, State},
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use utoipa_axum::router::OpenApiRouter;

use crate::common::AppState;
use crate::common::authz::{Capability, TokenAccess, TokenBit};
use crate::common::middleware::{ProjectScope, require_crud, require_read_metadata};
use crate::error::{AppError, AppResult};
use crate::routes::private::projects::Project;
use crate::routes::private::sites;
use crate::routes::private::sites::models::SiteProjection;
use crate::routes::resolve_project;

/// List sites belonging to a project
#[utoipa::path(
    get,
    path = "/api/projects/{project_id}/sites",
    params(
        ("project_id" = String, Path, description = "Project UUID or name"),
    ),
    responses(
        (status = 200, description = "Sites retrieved successfully", body = Vec<SiteProjection>),
        (status = 404, description = "Project not found"),
    ),
    tag = "projects"
)]
pub async fn list_project_sites(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<Vec<SiteProjection>>> {
    let project = resolve_project(&state.db, &project_id).await?;

    // Enforce project scope
    if !scope.allows_project(project.id) {
        return Err(AppError::Forbidden(
            "That project is outside your access".to_string(),
        ));
    }

    let sites_list = sites::Entity::find()
        .filter(sites::Column::ProjectId.eq(project.id))
        .order_by_asc(sites::Column::Name)
        .all(&state.db)
        .await?;

    let response: Vec<SiteProjection> = sites_list
        .into_iter()
        .map(|s| SiteProjection {
            id: s.id,
            project_id: s.project_id,
            subproject_id: s.subproject_id,
            name: s.name,
            latitude: s.latitude,
            longitude: s.longitude,
            altitude_m: s.altitude_m,
        })
        .collect();

    Ok(Json(response))
}

pub fn service_router(state: &AppState) -> OpenApiRouter {
    // Projects are the top-level grant boundary: human management is Administrator-only, but the
    // historical write_metadata token bit is preserved so discovery/tooling flows keep working.
    let crud = Project::router(&state.db).layer(middleware::from_fn(require_crud(
        Capability::ReadMetadata,
        Capability::Admin,
        TokenAccess::Bit(TokenBit::WriteMetadata),
    )));

    let custom = OpenApiRouter::new()
        .route("/{project_id}/sites", get(list_project_sites))
        .with_state(state.clone())
        .layer(middleware::from_fn(require_read_metadata));

    crud.merge(custom)
}

// --- The public config cache ---

/// What an invalidation answers with: the project whose cache was dropped.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct InvalidatedConfigResponse {
    /// `invalidated`, always.
    pub status: String,
    pub code: String,
}

/// Invalidate the in-memory cache for a public project's API config. Use after editing
/// public visibility settings to force a re-read on next public API request. Requires
/// `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/actions/invalidate_public_config/{code}",
    params(("code" = String, Path, description = "Public project code")),
    responses(
        (status = 200, description = "Cache invalidated", body = InvalidatedConfigResponse),
    ),
    tag = "actions"
)]
pub async fn invalidate_public_config(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> AppResult<Json<InvalidatedConfigResponse>> {
    invalidate_config(&state.public_config_cache, &code).await;
    Ok(Json(InvalidatedConfigResponse {
        status: "invalidated".to_string(),
        code,
    }))
}
