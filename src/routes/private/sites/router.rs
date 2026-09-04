use axum::middleware;
use axum::routing::get;
use utoipa_axum::router::OpenApiRouter;

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

    // Mounted by explicit path, nest-relative, while each handler's `#[utoipa::path]` declares
    // the absolute URL: the spec is assembled from those declarations, not from this router.
    let data = OpenApiRouter::new()
        .route(
            "/{site_id}/readings",
            get(super::readings::get_site_readings),
        )
        .route(
            "/{site_id}/aggregates/{resolution}",
            get(super::aggregates::get_site_aggregates),
        )
        .route(
            "/{site_id}/status_events",
            get(super::status_events::get_site_status_events),
        )
        .route(
            "/{site_id}/alarms",
            get(crate::routes::private::alarms::views::get_site_alarms),
        )
        .route(
            "/{site_id}/annotations",
            get(super::annotations::get_site_annotations),
        )
        .route(
            "/{site_id}/export/summary",
            get(super::annotations::get_site_export_summary),
        )
        .route(
            "/{site_id}/export/sensor-vs-grab",
            get(super::sensor_vs_grab::get_sensor_vs_grab),
        )
        .route(
            "/{site_id}/sensor_identity",
            get(super::sensor_identity::get_site_sensor_identity),
        )
        .route(
            "/{site_id}/last_curve",
            get(crate::routes::private::sensors::standard_curves::views::last_used_curve),
        )
        .with_state(state.clone())
        .layer(middleware::from_fn(require_read_data));

    let metadata = OpenApiRouter::new()
        .route(
            "/{site_id}/parameters",
            get(super::views::list_site_parameters),
        )
        .route("/{site_id}/detail", get(super::views::get_site_detail))
        .with_state(state.clone())
        .layer(middleware::from_fn(require_read_metadata));

    crud.merge(data).merge(metadata)
}
