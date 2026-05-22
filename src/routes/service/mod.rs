use axum::{Router, middleware, routing::{get, patch, post}};
use tower_http::limit::RequestBodyLimitLayer;
use utoipa_axum::router::OpenApiRouter;

use crate::common::AppState;
use crate::common::middleware::{
    require_crud_permissions, require_read_data, require_read_metadata, require_write_data,
    require_write_metadata,
};
use crate::common::rate_limit::FallbackIpKeyExtractor;
use crate::routes::private::{
    alarm_thresholds::AlarmThreshold,
    annotations::Annotation,
    api_tokens::ApiToken,
    constants::Constant,
    data_streams::DataStream,
    derived_parameters::definition_model::DerivedParameterDefinition,
    derived_parameters::source_model::DerivedParameterSource,
    notes::Note,
    pairing_plans::PairingPlan,
    parameters::Parameter,
    public_exposed_parameters::PublicExposedParameter,
    reprocessing_jobs::ReprocessingJob,
    samples::Sample,
    sensor_calibrations::SensorCalibration,
    sensor_deployments::SensorDeployment,
    sensors::Sensor,
    site_parameters::SiteParameter,
    standard_curves::StandardCurve,
    sync::commands_model::SyncCommand,
    sync::events_model::SyncEvent,
    sync::services_model::SyncService,
};

pub fn service_router(state: &AppState) -> Router<()> {
    let db = &state.db;

    let with_crud_perms = |r: OpenApiRouter| -> OpenApiRouter {
        r.layer(middleware::from_fn(require_crud_permissions))
    };

    let entity_router: Router<()> = OpenApiRouter::new()
        .nest("/projects", crate::routes::private::projects::router::service_router(state))
        .nest("/sites", crate::routes::private::sites::router::service_router(state))
        .nest("/parameters", with_crud_perms(Parameter::router(db)))
        .nest("/site_parameters", with_crud_perms(SiteParameter::router(db)))
        .nest("/sensors", with_crud_perms(Sensor::router(db)))
        .nest("/sensor_calibrations", with_crud_perms(SensorCalibration::router(db)))
        .nest("/sensor_deployments", with_crud_perms(SensorDeployment::router(db)))
        .nest("/derived_parameters", with_crud_perms(DerivedParameterDefinition::router(db)))
        .nest("/derived_parameter_sources", with_crud_perms(DerivedParameterSource::router(db)))
        .nest("/alarm_thresholds", with_crud_perms(AlarmThreshold::router(db)))
        .nest("/tokens", with_crud_perms(ApiToken::router(db)))
        .nest("/public_exposed_parameters", with_crud_perms(PublicExposedParameter::router(db)))
        .nest("/data_streams", with_crud_perms(DataStream::router(db)))
        .nest("/standard_curves", with_crud_perms(StandardCurve::router(db)))
        .nest("/notes", with_crud_perms(Note::router(db)))
        .nest("/annotations", with_crud_perms(Annotation::router(db)))
        .nest("/constants", with_crud_perms(Constant::router(db)))
        .nest("/samples", with_crud_perms(Sample::router(db)))
        .nest("/reprocessing_jobs", with_crud_perms(ReprocessingJob::router(db)))
        .nest("/sync_services", with_crud_perms(SyncService::router(db)))
        .nest("/sync_commands", with_crud_perms(SyncCommand::router(db)))
        .nest("/sync_events", with_crud_perms(SyncEvent::router(db)))
        .nest("/pairing_plans", with_crud_perms(PairingPlan::router(db)))
        .into();

    use crate::routes::private::{
        admin::actions,
        alarms::views as alarm_views,
        data_streams::views as stream_views,
        readings::{batch as readings_batch, flags, grab_samples, ingest},
        search,
        status_events::batch as status_events_batch,
        tools,
    };

    let stream_read_routes = Router::new()
        .route("/streams/{id}/stats", get(stream_views::stream_stats))
        .layer(middleware::from_fn(require_read_metadata))
        .with_state(state.clone());

    let stream_write_routes = Router::new()
        .route("/streams/register", post(stream_views::register_stream))
        .route("/streams/{id}/pair", post(stream_views::pair_stream))
        .route("/streams/{id}/unpair", post(stream_views::unpair_stream))
        .layer(middleware::from_fn(require_write_metadata))
        .with_state(state.clone());

    let data_write_routes = Router::new()
        .route("/ingest", post(ingest::ingest_readings))
        .route("/ingest/status_events", post(ingest::ingest_status_events))
        .route("/readings/batch", post(readings_batch::insert_batch_readings))
        .route("/status_events/batch", post(status_events_batch::insert_batch_status_events))
        .layer(RequestBodyLimitLayer::new(10 * 1024 * 1024))
        .route("/grab_samples", post(grab_samples::insert_grab_samples))
        .route("/readings/flag", patch(flags::flag_readings))
        .route("/readings/unflag", patch(flags::unflag_readings))
        .route("/actions/refresh_aggregates", post(actions::refresh_aggregates))
        .route("/actions/compute_derived", post(actions::compute_derived))
        .route("/actions/rollback_deployment", post(actions::rollback_deployment))
        .layer(middleware::from_fn(require_write_data))
        .with_state(state.clone());

    let data_read_routes = Router::new()
        .route("/actions/preview_derived", post(actions::preview_derived))
        .route("/alarms/active", get(alarm_views::get_active_alarms))
        .route("/alarms/summary", get(alarm_views::get_alarm_summary))
        .route("/tools", get(tools::list_tools))
        .route("/tools/{tool_name}/calculate", post(tools::calculate_tool))
        .layer(middleware::from_fn(require_read_data))
        .with_state(state.clone());

    let metadata_read_routes = Router::new()
        .route("/search", get(search::search))
        .layer(middleware::from_fn(require_read_metadata))
        .with_state(state.clone());

    Router::new()
        .merge(entity_router)
        .merge(stream_read_routes)
        .merge(stream_write_routes)
        .merge(metadata_read_routes)
        .merge(data_write_routes)
        .merge(data_read_routes)
}

pub fn sync_control_router(_state: &AppState) -> Router<AppState> {
    use std::sync::Arc;
    use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};

    let (service_routes, _admin_routes) = river_data_core::server::routes::<AppState>();

    let enroll_limiter = GovernorConfigBuilder::default()
        .key_extractor(FallbackIpKeyExtractor)
        .per_second(3)
        .burst_size(10)
        .finish()
        .expect("Failed to create enroll rate limiter");

    Router::new()
        .nest("/sync", service_routes)
        .layer(GovernorLayer {
            config: Arc::new(enroll_limiter),
        })
}
