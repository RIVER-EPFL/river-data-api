pub mod actions;
pub mod readings_batch;
pub mod source_mappings;

use axum::{Router, middleware, routing::{get, post, put}};
use tower_http::limit::RequestBodyLimitLayer;

use crate::common::AppState;
use crate::entity::{
    alarm_thresholds::AlarmThreshold,
    api_tokens::ApiToken,
    derived_parameter_definitions::DerivedParameterDefinition,
    parameters::Parameter,
    projects::Project,
    public_exposed_parameters::PublicExposedParameter,
    sensor_calibrations::SensorCalibration,
    sensor_deployments::SensorDeployment,
    sensors::Sensor,
    site_parameters::SiteParameter,
    sites::Site,
    sync_state::SyncState,
};

use super::{alarms, projects as projects_routes, sites as sites_routes};

/// Build the `/api/service/` router.
///
/// Combines CrudCrate CRUD routers (entity management) with hand-crafted
/// data endpoints (readings, aggregates, alarms) and new write endpoints
/// (batch readings, source mappings, sync state, action triggers).
///
/// Auth: Keycloak JWT OR API token (dual auth, same as old /api/private/).
/// Permissions are enforced per route group via middleware layers.
pub fn service_router(state: &AppState) -> Router<AppState> {
    let db = &state.db;

    // Convert crudcrate OpenApiRouters to axum::Router for nesting
    let crud = |r: utoipa_axum::router::OpenApiRouter| -> Router<()> { r.into() };

    // ========================================================================
    // CrudCrate CRUD routers — read requires read_metadata, write requires write_metadata
    // ========================================================================

    // CrudCrate routers handle GET/POST/PATCH/DELETE internally.
    // We apply read_metadata for the whole mount, then overlay write_metadata
    // for mutating methods using a method-based middleware.
    //
    // Since CrudCrate routers use nest_service (opaque services), we can't
    // selectively layer per-method. Instead, we split into read-only and
    // write-capable groups. CrudCrate mounts are read+write, so we apply
    // write_metadata to all CrudCrate routes (Keycloak users pass through).

    let crud_routes = Router::new()
        .nest_service("/projects", crud(Project::router(db)))
        .nest_service("/sites", crud(Site::router(db)))
        .nest_service("/parameters", crud(Parameter::router(db)))
        .nest_service("/site_parameters", crud(SiteParameter::router(db)))
        .nest_service("/sensors", crud(Sensor::router(db)))
        .nest_service("/sensor_calibrations", crud(SensorCalibration::router(db)))
        .nest_service("/sensor_deployments", crud(SensorDeployment::router(db)))
        .nest_service(
            "/derived_parameters",
            crud(DerivedParameterDefinition::router(db)),
        )
        .nest_service("/alarm_thresholds", crud(AlarmThreshold::router(db)))
        .nest_service("/tokens", crud(ApiToken::router(db)))
        .nest_service(
            "/public_exposed_parameters",
            crud(PublicExposedParameter::router(db)),
        )
        .nest_service("/sync_states", crud(SyncState::router(db)))
        .layer(middleware::from_fn(
            crate::common::middleware::require_crud_permissions,
        ));

    // ========================================================================
    // Hand-crafted metadata routes (from old /api/private/) — require read_metadata
    // ========================================================================

    let metadata_routes = Router::new()
        .route("/projects", get(projects_routes::list_projects))
        .route("/projects/{project_id}", get(projects_routes::get_project))
        .route(
            "/projects/{project_id}/sites",
            get(projects_routes::list_project_sites),
        )
        .route("/sites", get(sites_routes::list_sites))
        .route("/sites/{site_id}", get(sites_routes::get_site))
        .route(
            "/sites/{site_id}/parameters",
            get(sites_routes::list_site_parameters),
        )
        .layer(middleware::from_fn(
            crate::common::middleware::require_read_metadata,
        ));

    // ========================================================================
    // Hand-crafted data read routes (from old /api/private/) — require read_data
    // ========================================================================

    let data_read_routes = Router::new()
        .route(
            "/sites/{site_id}/readings",
            get(sites_routes::get_site_readings),
        )
        .route(
            "/sites/{site_id}/aggregates/{resolution}",
            get(sites_routes::get_site_aggregates),
        )
        .route(
            "/sites/{site_id}/alarms",
            get(alarms::get_site_alarms),
        )
        .layer(middleware::from_fn(
            crate::common::middleware::require_read_data,
        ));

    // ========================================================================
    // Source mappings — read requires read_metadata, write requires write_metadata
    // ========================================================================

    let source_mappings_read = Router::new()
        .route("/source_mappings", get(source_mappings::list_source_mappings))
        .layer(middleware::from_fn(
            crate::common::middleware::require_read_metadata,
        ));

    let source_mappings_write = Router::new()
        .route("/source_mappings", post(source_mappings::upsert_source_mapping))
        .route(
            "/source_mappings/{entity_type}/{source_key}",
            put(source_mappings::update_source_mapping),
        )
        .layer(middleware::from_fn(
            crate::common::middleware::require_write_metadata,
        ));

    // ========================================================================
    // Data write routes — require write_data
    // ========================================================================

    let data_write_routes = Router::new()
        .route(
            "/readings/batch",
            post(readings_batch::insert_batch_readings),
        )
        // Raise body limit to 10MB for batch inserts
        .layer(RequestBodyLimitLayer::new(10 * 1024 * 1024))
        .route(
            "/actions/refresh_aggregates",
            post(actions::refresh_aggregates),
        )
        .route(
            "/actions/compute_derived",
            post(actions::compute_derived),
        )
        .route(
            "/actions/update_last_full_sync",
            post(actions::update_last_full_sync),
        )
        .layer(middleware::from_fn(
            crate::common::middleware::require_write_data,
        ));

    // ========================================================================
    // Combine all service routes
    // ========================================================================

    Router::new()
        .merge(crud_routes)
        .merge(metadata_routes)
        .merge(data_read_routes)
        .merge(source_mappings_read)
        .merge(source_mappings_write)
        .merge(data_write_routes)
}
