use axum::middleware;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::common::AppState;
use crate::common::middleware::{require_crud_permissions, require_read_metadata};
use crate::entity::projects::Project;

pub fn service_router(state: &AppState) -> OpenApiRouter {
    let crud = Project::router(&state.db)
        .layer(middleware::from_fn(require_crud_permissions));

    let custom = OpenApiRouter::new()
        .routes(routes!(super::handlers::list_project_sites))
        .with_state(state.clone())
        .layer(middleware::from_fn(require_read_metadata));

    crud.merge(custom)
}
