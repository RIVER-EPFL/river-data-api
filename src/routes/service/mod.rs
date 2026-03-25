pub mod actions;
pub mod field_trips;
pub mod grab_samples;
pub mod ingest;
pub mod reading_flags;
pub mod readings_batch;
pub mod search;
pub mod status_events_batch;
pub mod streams;
pub mod sync_control;
pub mod tools;

use axum::{Router, middleware, routing::{get, patch, post}};
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
    data_streams::DataStream,
    derived_parameter_definitions::DerivedParameterDefinition,
    derived_parameter_sources::DerivedParameterSource,
    field_trips::FieldTrip,
    notes::Note,
    pairing_plans::PairingPlan,
    parameters::Parameter,
    public_exposed_parameters::PublicExposedParameter,
    sensor_calibrations::SensorCalibration,
    sensor_deployments::SensorDeployment,
    sensors::Sensor,
    site_parameters::SiteParameter,
    standard_curves::StandardCurve,
    sync_commands::SyncCommand,
    sync_events::SyncEvent,
    sync_services::SyncService,
};

/// Build the `/api/service/` router.
///
/// Combines `CrudCrate` CRUD routers (entity management) with hand-crafted
/// data endpoints (readings, aggregates, alarms) and stream-based ingestion.
///
/// Auth: Keycloak JWT OR API token (dual auth).
/// Permissions are enforced per route group via middleware layers.
pub fn service_router(state: &AppState) -> Router<()> {
    let db = &state.db;

    let with_crud_perms = |r: OpenApiRouter| -> OpenApiRouter {
        r.layer(middleware::from_fn(require_crud_permissions))
    };

    // ========================================================================
    // Entity routers — CrudCrate CRUD + hand-crafted custom routes
    // ========================================================================

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
        .nest(
            "/data_streams",
            with_crud_perms(DataStream::router(db)),
        )
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
        .nest(
            "/sync_services",
            with_crud_perms(SyncService::router(db)),
        )
        .nest(
            "/sync_commands",
            with_crud_perms(SyncCommand::router(db)),
        )
        .nest(
            "/sync_events",
            with_crud_perms(SyncEvent::router(db)),
        )
        .nest(
            "/pairing_plans",
            with_crud_perms(PairingPlan::router(db)),
        )
        .into();

    // ========================================================================
    // Stream routes — registration, listing, pair/unpair
    // ========================================================================

    let stream_read_routes = Router::new()
        .route("/streams", get(streams::list_streams))
        .route("/streams/{id}", get(streams::get_stream))
        .route("/streams/{id}/stats", get(streams::stream_stats))
        .layer(middleware::from_fn(require_read_metadata))
        .with_state(state.clone());

    let stream_write_routes = Router::new()
        .route("/streams/register", post(streams::register_stream))
        .route("/streams/{id}/pair", post(streams::pair_stream))
        .route("/streams/{id}/unpair", post(streams::unpair_stream))
        .layer(middleware::from_fn(require_write_metadata))
        .with_state(state.clone());

    // ========================================================================
    // Data write routes — require write_data
    // ========================================================================

    let data_write_routes = Router::new()
        .route("/ingest", post(ingest::ingest_readings))
        .route(
            "/ingest/status_events",
            post(ingest::ingest_status_events),
        )
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
        .route("/tools", get(tools::list_tools))
        .route(
            "/tools/{tool_name}/calculate",
            post(tools::calculate_tool),
        )
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
        .merge(stream_read_routes)
        .merge(stream_write_routes)
        .merge(metadata_read_routes)
        .merge(data_write_routes)
        .merge(data_read_routes)
}

/// Build the sync control plane routes.
///
/// These are mounted under `/api/service/` but bypass `service_auth_middleware`
/// because they use their own auth mechanisms:
/// - Enroll: `client_id` + `client_secret` in the JSON body
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
        .route(
            "/sync/events",
            post(sync_control::create_sync_event),
        )
        .route(
            "/sync/events/{id}",
            axum::routing::patch(sync_control::update_sync_event),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::common::middleware::sync_service_auth_middleware,
        ));

    Router::new()
        .merge(sync_enroll_routes)
        .merge(sync_authenticated_routes)
}
