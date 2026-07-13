use axum::middleware;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::common::AppState;
use crate::common::authz::{Capability, TokenAccess};
use crate::common::middleware::{require_crud, require_read_data, require_read_metadata};
use crate::routes::private::sites::Site;

pub fn service_router(state: &AppState) -> OpenApiRouter {
    // Sites are field metadata: RIVER members (and write_metadata tokens) may create/edit them.
    let crud = Site::router(&state.db).layer(middleware::from_fn(require_crud(
        Capability::ReadMetadata,
        Capability::WriteFieldMetadata,
        TokenAccess::Same,
    )));

    let data = OpenApiRouter::new()
        .routes(routes!(super::readings::get_site_readings))
        .routes(routes!(super::aggregates::get_site_aggregates))
        .routes(routes!(super::status_events::get_site_status_events))
        .routes(routes!(crate::routes::private::alarms::views::get_site_alarms))
        .routes(routes!(super::annotations::get_site_annotations))
        .routes(routes!(super::sensor_vs_grab::get_sensor_vs_grab))
        .routes(routes!(super::sensor_identity::get_site_sensor_identity))
        .with_state(state.clone())
        .layer(middleware::from_fn(require_read_data));

    let metadata = OpenApiRouter::new()
        .routes(routes!(super::views::list_site_parameters))
        .routes(routes!(super::views::get_site_detail))
        .with_state(state.clone())
        .layer(middleware::from_fn(require_read_metadata));

    crud.merge(data).merge(metadata)
}
