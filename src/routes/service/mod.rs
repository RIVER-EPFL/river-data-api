pub mod actions;
pub mod field_trips;
pub mod grab_samples;
pub mod reading_flags;
pub mod readings_batch;
pub mod search;
pub mod source_mappings;
pub mod status_events_batch;
pub mod sync_control;

use axum::{Router, middleware, routing::{get, patch, post, put}};
use tower_http::limit::RequestBodyLimitLayer;
use utoipa_axum::router::OpenApiRouter;

use crate::common::AppState;
use crate::common::middleware::{
    require_crud_permissions, require_read_data, require_read_metadata, require_write_data,
    require_write_metadata,
};
use crate::entity::{
    alarm_thresholds::AlarmThreshold,
    annotations::Annotation,
    api_tokens::ApiToken,
    constants::Constant,
    derived_parameter_definitions::DerivedParameterDefinition,
    derived_parameter_sources::DerivedParameterSource,
    field_trips::FieldTrip,
    notes::Note,
    parameters::Parameter,
    public_exposed_parameters::PublicExposedParameter,
    sensor_calibrations::SensorCalibration,
    sensor_deployments::SensorDeployment,
    sensors::Sensor,
    site_parameters::SiteParameter,
    standard_curves::StandardCurve,
    sync_state::SyncState,
};

/// Build the `/api/service/` router.
///
/// Combines CrudCrate CRUD routers (entity management) with hand-crafted
/// data endpoints (readings, aggregates, alarms) and new write endpoints
/// (batch readings, source mappings, sync state, action triggers).
///
/// Auth: Keycloak JWT OR API token (dual auth, same as old /api/private/).
/// Permissions are enforced per route group via middleware layers.
///
/// Uses OpenApiRouter + nest() to avoid the catch-all wildcards that
/// nest_service() creates, which conflict with hand-crafted routes.
pub fn service_router(state: &AppState) -> Router<()> {
    let db = &state.db;

    let with_crud_perms = |r: OpenApiRouter| -> OpenApiRouter {
        r.layer(middleware::from_fn(require_crud_permissions))
    };

    // ========================================================================
    // Entity routers — CrudCrate CRUD + hand-crafted custom routes
    // ========================================================================
    //
    // Projects and Sites use per-entity views that merge CrudCrate CRUD
    // routes with custom endpoints (e.g., /{id}/sites, /{id}/readings).
    // Other entities use CrudCrate directly with require_crud_permissions.
    //
    // OpenApiRouter.nest() adds prefixed individual routes (no catch-all),
    // so CrudCrate's GET /{id} coexists with custom GET /{id}/readings.

    let entity_router: Router<()> = OpenApiRouter::new()
        .nest("/projects", super::projects::views::service_router(state))
        .nest("/sites", super::sites::views::service_router(state))
        .nest("/parameters", with_crud_perms(Parameter::router(db)))
        .nest(
            "/site_parameters",
            with_crud_perms(SiteParameter::router(db)),
        )
        .nest("/sensors", with_crud_perms(Sensor::router(db)))
        .nest(
            "/sensor_calibrations",
            with_crud_perms(SensorCalibration::router(db)),
        )
        .nest(
            "/sensor_deployments",
            with_crud_perms(SensorDeployment::router(db)),
        )
        .nest(
            "/derived_parameters",
            with_crud_perms(DerivedParameterDefinition::router(db)),
        )
        .nest(
            "/derived_parameter_sources",
            with_crud_perms(DerivedParameterSource::router(db)),
        )
        .nest(
            "/alarm_thresholds",
            with_crud_perms(AlarmThreshold::router(db)),
        )
        .nest("/tokens", with_crud_perms(ApiToken::router(db)))
        .nest(
            "/public_exposed_parameters",
            with_crud_perms(PublicExposedParameter::router(db)),
        )
        .nest("/sync_states", with_crud_perms(SyncState::router(db)))
        .nest(
            "/standard_curves",
            with_crud_perms(StandardCurve::router(db)),
        )
        .nest("/notes", with_crud_perms(Note::router(db)))
        .nest(
            "/annotations",
            with_crud_perms(Annotation::router(db)),
        )
        .nest("/constants", with_crud_perms(Constant::router(db)))
        .nest("/field_trips", with_crud_perms(FieldTrip::router(db)))
        .into();

    // ========================================================================
    // Source mappings — read requires read_metadata, write requires write_metadata
    // ========================================================================

    let source_mappings_read = Router::new()
        .route("/source_mappings", get(source_mappings::list_source_mappings))
        .layer(middleware::from_fn(require_read_metadata))
        .with_state(state.clone());

    let source_mappings_write = Router::new()
        .route(
            "/source_mappings",
            post(source_mappings::upsert_source_mapping),
        )
        .route(
            "/source_mappings/{entity_type}/{source_key}",
            put(source_mappings::update_source_mapping),
        )
        .layer(middleware::from_fn(require_write_metadata))
        .with_state(state.clone());

    // ========================================================================
    // Data write routes — require write_data
    // ========================================================================

    let data_write_routes = Router::new()
        .route(
            "/readings/batch",
            post(readings_batch::insert_batch_readings),
        )
        .route(
            "/status_events/batch",
            post(status_events_batch::insert_batch_status_events),
        )
        // Raise body limit to 10MB for batch inserts
        .layer(RequestBodyLimitLayer::new(10 * 1024 * 1024))
        .route(
            "/grab_samples",
            post(grab_samples::insert_grab_samples),
        )
        .route(
            "/actions/field_trip_batch",
            post(field_trips::create_field_trip_batch),
        )
        .route(
            "/readings/flag",
            patch(reading_flags::flag_readings),
        )
        .route(
            "/readings/unflag",
            patch(reading_flags::unflag_readings),
        )
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
        .layer(middleware::from_fn(require_write_data))
        .with_state(state.clone());

    // ========================================================================
    // Data read routes — require read_data
    // ========================================================================

    let data_read_routes = Router::new()
        .route(
            "/actions/preview_derived",
            post(actions::preview_derived),
        )
        .route("/alarms/active", get(super::alarms::get_active_alarms))
        .route("/alarms/summary", get(super::alarms::get_alarm_summary))
        .layer(middleware::from_fn(require_read_data))
        .with_state(state.clone());

    // ========================================================================
    // Metadata read routes — require read_metadata
    // ========================================================================

    let metadata_read_routes = Router::new()
        .route("/search", get(search::search))
        .layer(middleware::from_fn(require_read_metadata))
        .with_state(state.clone());

    // ========================================================================
    // Combine all service routes
    // ========================================================================

    Router::new()
        .merge(entity_router)
        .merge(source_mappings_read)
        .merge(source_mappings_write)
        .merge(metadata_read_routes)
        .merge(data_write_routes)
        .merge(data_read_routes)
}

/// Build the sync control plane routes.
///
/// These are mounted under `/api/service/` but bypass `service_auth_middleware`
/// because they use their own auth mechanisms:
/// - Enroll: client_id + client_secret in the JSON body
/// - Heartbeat/commands: sync session token via `sync_service_auth_middleware`
///
/// Returns `Router<AppState>` so the caller can nest it alongside other
/// `AppState`-typed routers; `with_state()` is applied by `build_router()`.
pub fn sync_control_router(state: &AppState) -> Router<AppState> {
    let sync_enroll_routes: Router<AppState> = Router::new()
        .route("/sync/enroll", post(sync_control::enroll));

    let sync_authenticated_routes: Router<AppState> = Router::new()
        .route("/sync/heartbeat", post(sync_control::heartbeat))
        .route(
            "/sync/commands/{id}",
            axum::routing::patch(sync_control::update_command),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::common::middleware::sync_service_auth_middleware,
        ));

    Router::new()
        .merge(sync_enroll_routes)
        .merge(sync_authenticated_routes)
}
