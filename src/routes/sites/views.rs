use axum::middleware;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::common::AppState;
use crate::common::middleware::{require_crud_permissions, require_read_data, require_read_metadata};
use crate::entity::sites::Site;

pub fn service_router(state: &AppState) -> OpenApiRouter {
    let crud = Site::router(&state.db)
        .layer(middleware::from_fn(require_crud_permissions));

    let data = OpenApiRouter::new()
        .routes(routes!(super::readings::get_site_readings))
        .routes(routes!(super::aggregates::get_site_aggregates))
        .routes(routes!(super::status_events::get_site_status_events))
        .routes(routes!(crate::routes::alarms::get_site_alarms))
        .with_state(state.clone())
        .layer(middleware::from_fn(require_read_data));

    let metadata = OpenApiRouter::new()
        .routes(routes!(super::handlers::list_site_parameters))
        .with_state(state.clone())
        .layer(middleware::from_fn(require_read_metadata));

    crud.merge(data).merge(metadata)
}
