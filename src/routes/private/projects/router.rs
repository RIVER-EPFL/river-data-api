use axum::middleware;
use axum::routing::get;
use utoipa_axum::router::OpenApiRouter;

use crate::common::AppState;
use crate::common::authz::{Capability, TokenAccess, TokenBit};
use crate::common::middleware::{require_crud, require_read_metadata};
use crate::routes::private::projects::Project;

pub fn service_router(state: &AppState) -> OpenApiRouter {
    // Projects are the top-level grant boundary: human management is Administrator-only, but the
    // historical write_metadata token bit is preserved so discovery/tooling flows keep working.
    let crud = Project::router(&state.db).layer(middleware::from_fn(require_crud(
        Capability::ReadMetadata,
        Capability::Admin,
        TokenAccess::Bit(TokenBit::WriteMetadata),
    )));

    let custom = OpenApiRouter::new()
        .route("/{project_id}/sites", get(super::views::list_project_sites))
        .with_state(state.clone())
        .layer(middleware::from_fn(require_read_metadata));

    crud.merge(custom)
}
