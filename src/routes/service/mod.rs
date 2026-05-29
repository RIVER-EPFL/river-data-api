use axum::{Router, middleware, routing::{get, patch, post}};
use tower_http::limit::RequestBodyLimitLayer;
use utoipa_axum::router::OpenApiRouter;

use crate::common::AppState;
use crate::common::middleware::{
    require_admin, require_crud_permissions, require_read_data, require_read_metadata,
    require_write_data, require_write_metadata,
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
    sync::credentials_model::SyncServiceCredential,
    sync::events_model::SyncEvent,
    sync::services_model::SyncService,
};

const ACTION_BODY_LIMIT: usize = 1024 * 1024; // 1 MB — preserved from the former admin tier
const DATA_BODY_LIMIT: usize = 10 * 1024 * 1024; // 10 MB — bulk ingestion only

/// The single `/api/` router. Mounted by the parent router which wraps it with
/// dual-auth (`service_auth_middleware`), Keycloak JWT pass-through, and optional
/// rate limiting. Per-route authorization is enforced inside this function via
/// the `require_*` middleware on grouped sub-routers.
pub fn api_router(state: &AppState) -> Router<()> {
    let db = &state.db;

    let with_crud_perms = |r: OpenApiRouter| -> OpenApiRouter {
        r.layer(middleware::from_fn(require_crud_permissions))
    };
    let admin_only_crud = |r: OpenApiRouter| -> OpenApiRouter {
        // CrudCrate exposes a single router for all 5 methods. For entities that mint
        // privileged credentials (API tokens, sync service credentials) we keep the
        // entire surface — including LIST/GET — behind require_admin so leaked tokens
        // can't enumerate credentials. See plan: defense in depth.
        r.layer(middleware::from_fn(require_admin))
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
        .nest("/tokens", admin_only_crud(ApiToken::router(db)))
        .nest("/sync_service_credentials", admin_only_crud(SyncServiceCredential::router(db)))
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
        admin::{actions, calibrations, derived, merge, public_config, users},
        alarms::views as alarm_views,
        data_streams::views as stream_views,
        readings::{batch as readings_batch, flags, grab_samples, import as readings_import, ingest},
        search,
        status_events::batch as status_events_batch,
        sync::views as sync_views,
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
        .route("/readings/import_csv", post(readings_import::import_csv))
        .route("/status_events/batch", post(status_events_batch::insert_batch_status_events))
        .layer(RequestBodyLimitLayer::new(DATA_BODY_LIMIT))
        .route("/grab_samples", post(grab_samples::insert_grab_samples))
        .route("/readings/flag", patch(flags::flag_readings))
        .route("/readings/unflag", patch(flags::unflag_readings))
        .route("/readings/flag_range", patch(flags::flag_range))
        .route("/readings/unflag_range", patch(flags::unflag_range))
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

    // Operator actions previously on /api/admin/. require_write_metadata already
    // enforces "Keycloak admin OR API token with write_metadata" — appropriate for
    // automation scripts driving calibration/recompute/merge workflows.
    let operator_action_routes = Router::new()
        .route(
            "/actions/sensor_calibrations/{id}/recalculate",
            post(calibrations::recalculate_calibration),
        )
        .route(
            "/actions/derived_parameters/{id}/recompute",
            post(derived::recompute_derived),
        )
        .route(
            "/actions/invalidate_public_config/{slug}",
            post(public_config::invalidate_public_config),
        )
        .route(
            "/actions/merge_site_parameters",
            post(merge::merge_site_parameters_handler),
        )
        .route(
            "/actions/merge_parameters",
            post(merge::merge_parameters_handler),
        )
        .layer(RequestBodyLimitLayer::new(ACTION_BODY_LIMIT))
        .layer(middleware::from_fn(require_write_metadata))
        .with_state(state.clone());

    // Sync admin views split by required permission. Credential creation/revoke is
    // require_admin because it mints full-permission sync session tokens — a token
    // with write_metadata must not be able to bootstrap a more privileged token.
    let sync_admin_read = Router::new()
        .nest("/sync", sync_views::read_routes())
        .layer(middleware::from_fn(require_read_metadata))
        .with_state(state.clone());

    let sync_admin_write = Router::new()
        .nest("/sync", sync_views::write_routes())
        .layer(RequestBodyLimitLayer::new(ACTION_BODY_LIMIT))
        .layer(middleware::from_fn(require_write_metadata))
        .with_state(state.clone());

    let sync_admin_admin = Router::new()
        .nest("/sync", sync_views::admin_routes())
        .layer(RequestBodyLimitLayer::new(ACTION_BODY_LIMIT))
        .layer(middleware::from_fn(require_admin))
        .with_state(state.clone());

    // Keycloak user management proxy. Conditional — only mounted if AppState has
    // admin client credentials configured. Strictly Keycloak admin: NO API token,
    // even one with full permissions, can pass require_admin.
    let user_routes = state.keycloak_admin.as_ref().map(|_| {
        Router::new()
            .nest("/users", users::router())
            .route("/roles", get(users::list_roles))
            .layer(middleware::from_fn(require_admin))
            .with_state(state.clone())
    });

    let mut router = Router::new()
        .merge(entity_router)
        .merge(stream_read_routes)
        .merge(stream_write_routes)
        .merge(metadata_read_routes)
        .merge(data_write_routes)
        .merge(data_read_routes)
        .merge(operator_action_routes)
        .merge(sync_admin_read)
        .merge(sync_admin_write)
        .merge(sync_admin_admin);

    if let Some(routes) = user_routes {
        router = router.merge(routes);
    }

    router
}

/// Legacy alias retained while routes/mod.rs is being updated to call api_router directly.
pub fn service_router(state: &AppState) -> Router<()> {
    api_router(state)
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
