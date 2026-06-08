use axum::{
    Router, middleware,
    extract::{Request, State},
    response::Response,
    routing::{get, patch, post},
};
use tower_http::limit::RequestBodyLimitLayer;
use utoipa_axum::router::OpenApiRouter;

use crate::common::AppState;
use crate::common::middleware::{
    bust_token_cache_on_mutation, deny_scoped_token, enforce_token_scope_on_crud, require_admin,
    require_crud_permissions, require_read_data, require_read_metadata, require_write_data,
    require_write_metadata,
};
use crate::common::rate_limit::FallbackIpKeyExtractor;
use crate::routes::private::{
    alarm_thresholds::AlarmThreshold,
    annotations::Annotation,
    api_token_audit_log::ApiTokenAuditLog,
    api_tokens::ApiToken,
    constants::Constant,
    data_streams::DataStream,
    derived_parameters::definition_model::DerivedParameterDefinition,
    derived_parameters::source_model::DerivedParameterSource,
    notes::Note,
    pairing_plans::PairingPlan,
    parameters::Parameter,
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
const DATA_BODY_LIMIT: usize = 10 * 1024 * 1024; // 10 MB — bulk ingestion
const IMPORT_BODY_LIMIT: usize = 50 * 1024 * 1024; // 50 MB — CSV import

/// Clear the public API config cache after a successful mutating request.
///
/// Layered onto the projects/sites/site_parameters CRUD routers — the entities the
/// public config (`public_config_cache`) is built from. Deliberately coarse: it drops
/// the whole cache rather than resolving the affected project code, since the cache is
/// a read-through convenience that rebuilds on the next public request, not a source of
/// truth. GET/HEAD requests and failed mutations leave it untouched.
async fn invalidate_public_config_on_mutation(
    State(state): State<AppState>,
    request: Request,
    next: middleware::Next,
) -> Response {
    let is_mutation = !matches!(request.method().as_str(), "GET" | "HEAD" | "OPTIONS");
    let response = next.run(request).await;
    if is_mutation && response.status().is_success() {
        state.public_config_cache.invalidate_all();
        tracing::debug!("Public API config cache cleared after entity mutation");
    }
    response
}

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
    // Clear the public API config cache whenever a project/site/site_parameter is
    // created, updated, or deleted. Coarse and best-effort — see the middleware doc.
    let invalidate_public_config = |r: OpenApiRouter| -> OpenApiRouter {
        r.layer(middleware::from_fn_with_state(
            state.clone(),
            invalidate_public_config_on_mutation,
        ))
    };

    let entity_router: Router<()> = OpenApiRouter::new()
        .nest("/projects", invalidate_public_config(crate::routes::private::projects::router::service_router(state)))
        .nest("/sites", invalidate_public_config(crate::routes::private::sites::router::service_router(state)))
        .nest("/parameters", with_crud_perms(Parameter::router(db)))
        .nest("/site_parameters", invalidate_public_config(with_crud_perms(SiteParameter::router(db))))
        .nest("/sensors", with_crud_perms(Sensor::router(db)))
        .nest("/sensor_calibrations", with_crud_perms(SensorCalibration::router(db)))
        .nest("/sensor_deployments", with_crud_perms(SensorDeployment::router(db)))
        .nest("/derived_parameters", with_crud_perms(DerivedParameterDefinition::router(db)))
        .nest("/derived_parameter_sources", with_crud_perms(DerivedParameterSource::router(db)))
        .nest("/alarm_thresholds", with_crud_perms(AlarmThreshold::router(db)))
        .nest(
            "/tokens",
            admin_only_crud(ApiToken::router(db)).layer(middleware::from_fn_with_state(
                state.clone(),
                bust_token_cache_on_mutation,
            )),
        )
        .nest("/sync_service_credentials", admin_only_crud(SyncServiceCredential::router(db)))
        // Read-only forensic audit trail of API-token use. Admin-only (no token can read it).
        .nest("/api_token_audit_logs", admin_only_crud(ApiTokenAuditLog::router(db)))
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
        // Project-scoped API tokens may only mutate project-bound entities within their project
        // (fails closed on the global catalog). No-op for Keycloak users and unscoped tokens.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            enforce_token_scope_on_crud,
        ))
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

    // Plain-handler sensor/calibration read views (not CrudCrate routes) for the plot overlays.
    let sensor_view_read_routes = Router::new()
        .route(
            "/sensors/{id}/readings",
            get(crate::routes::private::sensors::readings::get_sensor_readings),
        )
        .route(
            "/sensors/{id}/deployment_bands",
            get(crate::routes::private::sensors::readings::get_sensor_deployment_bands),
        )
        .route(
            "/sensor_calibrations/{id}/window",
            get(crate::routes::private::sensor_calibrations::window::get_calibration_window),
        )
        .layer(middleware::from_fn(require_read_data))
        .with_state(state.clone());

    let stream_write_routes = Router::new()
        .route("/streams/register", post(stream_views::register_stream))
        .route("/streams/{id}/import", post(stream_views::import_stream))
        .route("/streams/{id}/pair", post(stream_views::pair_stream))
        .route("/streams/{id}/unpair", post(stream_views::unpair_stream))
        .layer(middleware::from_fn(deny_scoped_token))
        .layer(middleware::from_fn(require_write_metadata))
        .with_state(state.clone());

    use crate::routes::private::sensors::adopt as sensor_adopt;
    let sensor_adopt_read = Router::new()
        .route(
            "/sensors/{sensor_id}/adopt_suggestions",
            get(sensor_adopt::adopt_suggestions),
        )
        .layer(middleware::from_fn(require_read_metadata))
        .with_state(state.clone());
    let sensor_adopt_write = Router::new()
        .route("/sensors/{sensor_id}/adopt", post(sensor_adopt::adopt_sensor))
        .route("/actions/swap", post(sensor_adopt::swap_sensors))
        .layer(middleware::from_fn(deny_scoped_token))
        .layer(middleware::from_fn(require_write_metadata))
        .with_state(state.clone());

    // Data push paths. Each handler self-enforces project scope (a scoped token may only write
    // within its project), so these stay reachable by per-client logger keys.
    let data_push_routes = Router::new()
        .route("/ingest", post(ingest::ingest_readings))
        .route("/ingest/status_events", post(ingest::ingest_status_events))
        .route("/readings/batch", post(readings_batch::insert_batch_readings))
        .route("/status_events/batch", post(status_events_batch::insert_batch_status_events))
        .layer(RequestBodyLimitLayer::new(DATA_BODY_LIMIT))
        .route("/readings/import_csv", post(readings_import::import_csv))
        .layer(axum::extract::DefaultBodyLimit::max(IMPORT_BODY_LIMIT))
        .route("/grab_samples", post(grab_samples::insert_grab_samples))
        .route("/readings/flag", patch(flags::flag_readings))
        .route("/readings/unflag", patch(flags::unflag_readings))
        .route("/readings/flag_range", patch(flags::flag_range))
        .route("/readings/unflag_range", patch(flags::unflag_range))
        .layer(middleware::from_fn(require_write_data))
        .with_state(state.clone());

    // Operator / global data actions that span projects or have no per-project target. Denied to
    // project-scoped tokens (a logger key has no reason to trigger a global reprocess/refresh).
    let data_action_routes = Router::new()
        .route("/actions/refresh_aggregates", post(actions::refresh_aggregates))
        .route("/actions/compute_derived", post(actions::compute_derived))
        .route("/actions/rollback_deployment", post(actions::rollback_deployment))
        .route("/actions/reprocess_all", post(actions::reprocess_all))
        .route("/actions/rebuild_alarm_events", post(actions::rebuild_alarm_events))
        .route("/actions/backfill_attribution", post(actions::backfill_attribution))
        .route("/actions/backfill_calibrations", post(actions::backfill_calibrations))
        .route("/alarms/{event_id}/acknowledge", post(alarm_views::acknowledge_alarm).delete(alarm_views::unacknowledge_alarm))
        .layer(middleware::from_fn(deny_scoped_token))
        .layer(middleware::from_fn(require_write_data))
        .with_state(state.clone());

    let data_read_routes = Router::new()
        .route("/actions/preview_derived", post(actions::preview_derived))
        .route("/alarms/active", get(alarm_views::get_active_alarms))
        .route("/alarms/summary", get(alarm_views::get_alarm_summary))
        .route("/alarms/events", get(alarm_views::get_alarm_events))
        .route("/events", get(crate::routes::private::events::event_stream))
        .route("/tools", get(tools::list_tools))
        .route("/tools/{tool_name}/calculate", post(tools::calculate_tool))
        .layer(middleware::from_fn(require_read_data))
        .with_state(state.clone());

    let metadata_read_routes = Router::new()
        .route("/search", get(search::search))
        .route("/actions/backfill_candidates", get(actions::backfill_candidates))
        .route("/actions/calibration_candidates", get(actions::calibration_candidates))
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
        .route("/actions/reprocess", post(actions::reprocess_sensor))
        .route(
            "/actions/derived_parameters/{id}/recompute",
            post(derived::recompute_derived),
        )
        .route(
            "/actions/invalidate_public_config/{code}",
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
        .layer(middleware::from_fn(deny_scoped_token))
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
        .layer(middleware::from_fn(deny_scoped_token))
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

    // Token lifecycle actions (revoke/rotate). Admin-only, like all token management.
    let token_admin_routes = Router::new()
        .route(
            "/tokens/{id}/revoke",
            post(crate::routes::private::api_tokens::views::revoke_token),
        )
        .route(
            "/tokens/{id}/rotate",
            post(crate::routes::private::api_tokens::views::rotate_token),
        )
        .route(
            "/tokens/{id}/usage",
            get(crate::routes::private::api_tokens::views::token_usage),
        )
        .layer(middleware::from_fn(require_admin))
        .with_state(state.clone());

    let mut router = Router::new()
        .merge(entity_router)
        .merge(token_admin_routes)
        .merge(stream_read_routes)
        .merge(sensor_view_read_routes)
        .merge(stream_write_routes)
        .merge(sensor_adopt_read)
        .merge(sensor_adopt_write)
        .merge(metadata_read_routes)
        .merge(data_push_routes)
        .merge(data_action_routes)
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
