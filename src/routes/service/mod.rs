use axum::{
    Router, middleware,
    extract::{Request, State},
    response::Response,
    routing::{delete, get, patch, post, put},
};
use tower_http::limit::RequestBodyLimitLayer;
use utoipa_axum::router::OpenApiRouter;

use crate::common::AppState;
use crate::common::authz::{Capability, TokenAccess, TokenBit};
use crate::common::middleware::{
    bust_token_cache_on_mutation, deny_scoped_token, enforce_scope_on_crud, inject_read_scope,
    require_admin, require_admin_or_token_write_metadata, require_crud, require_manage_sensors,
    require_read_data, require_read_metadata, require_write_data,
};
use crate::common::rate_limit::FallbackIpKeyExtractor;
use crate::routes::private::{
    alarm_thresholds::AlarmThreshold,
    annotations::Annotation,
    api_tokens::ApiToken,
    api_tokens::audit_log::ApiTokenAuditLog,
    constants::Constant,
    data_streams::DataStream,
    derived_parameters::definition_model::DerivedParameterDefinition,
    derived_parameters::source_model::DerivedParameterSource,
    notes::Note,
    notifications::{NotificationLog, NotificationMute, TelegramIdentity},
    pairing_plans::PairingPlan,
    parameters::Parameter,
    reprocessing_jobs::ReprocessingJob,
    samples::Sample,
    sensors::Sensor,
    sensors::calibrations::SensorCalibration,
    sensors::deployments::SensorDeployment,
    site_parameters::SiteParameter,
    subprojects::Subproject,
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

    // Per-entity CRUD gates: GET/HEAD need the read capability, mutations the write capability.
    // The token side stays frozen at the historical `write_metadata` bit (via `TokenAccess::Same`
    // for the split write capabilities, or an explicit `Bit` where the human side is admin-only)
    // so API tokens and sync-service session tokens are unaffected by the human-role RBAC.
    //
    // Field metadata (sites, site_parameters, standard curves, notes): RIVER members may write.
    let field_crud = |r: OpenApiRouter| -> OpenApiRouter {
        r.layer(middleware::from_fn(require_crud(
            Capability::ReadMetadata,
            Capability::WriteFieldMetadata,
            TokenAccess::Same,
        )))
    };
    // Field metadata whose rows are time-series data (annotations, samples): reads need read_data.
    let field_data_crud = |r: OpenApiRouter| -> OpenApiRouter {
        r.layer(middleware::from_fn(require_crud(
            Capability::ReadData,
            Capability::WriteFieldMetadata,
            TokenAccess::Same,
        )))
    };
    // Sensor movement (calibrations, deployments): MANAGER members may write.
    let sensor_crud = |r: OpenApiRouter| -> OpenApiRouter {
        r.layer(middleware::from_fn(require_crud(
            Capability::ReadMetadata,
            Capability::ManageSensors,
            TokenAccess::Same,
        )))
    };
    // Global catalog (parameters, derived definitions/sources, alarm thresholds, constants,
    // notification mutes): MANAGER members may write.
    let catalog_crud = |r: OpenApiRouter| -> OpenApiRouter {
        r.layer(middleware::from_fn(require_crud(
            Capability::ReadMetadata,
            Capability::WriteCatalog,
            TokenAccess::Same,
        )))
    };
    // Admin-managed inventory/system entities (sensor onboarding, data streams, reprocessing
    // jobs): human writes are Administrator-only, but the historical write_metadata token bit is
    // preserved so sync-service session tokens (which register streams and auto-create sensors)
    // keep working.
    let admin_write_crud = |r: OpenApiRouter| -> OpenApiRouter {
        r.layer(middleware::from_fn(require_crud(
            Capability::ReadMetadata,
            Capability::Admin,
            TokenAccess::Bit(TokenBit::WriteMetadata),
        )))
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
        // Global catalog (the shared parameter list, constants, derived definitions) is
        // Administrator-managed: onboarding a new global parameter is an admin act. Managers
        // instead ASSIGN parameters to sites (site_parameters) and manage per-site alarm thresholds.
        .nest("/parameters", admin_write_crud(Parameter::router(db)))
        .nest("/site_parameters", invalidate_public_config(catalog_crud(SiteParameter::router(db))))
        .nest("/sensors", admin_write_crud(Sensor::router(db)))
        .nest("/sensor_calibrations", sensor_crud(SensorCalibration::router(db)))
        .nest("/sensor_deployments", sensor_crud(SensorDeployment::router(db)))
        .nest("/derived_parameters", admin_write_crud(DerivedParameterDefinition::router(db)))
        .nest("/derived_parameter_sources", admin_write_crud(DerivedParameterSource::router(db)))
        .nest("/alarm_thresholds", catalog_crud(AlarmThreshold::router(db)))
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
        .nest("/data_streams", admin_write_crud(DataStream::router(db)))
        .nest("/subprojects", invalidate_public_config(field_crud(Subproject::router(db))))
        .nest("/notes", field_crud(Note::router(db)))
        .nest("/telegram_identities", admin_only_crud(TelegramIdentity::router(db)))
        .nest("/notification_mutes", catalog_crud(NotificationMute::router(db)))
        .nest("/notification_logs", admin_only_crud(NotificationLog::router(db)))
        .nest("/annotations", field_data_crud(Annotation::router(db)))
        .nest("/constants", catalog_crud(Constant::router(db)))
        .nest("/samples", field_data_crud(Sample::router(db)))
        .nest("/reprocessing_jobs", admin_write_crud(ReprocessingJob::router(db)))
        .nest("/sync_services", admin_only_crud(SyncService::router(db)))
        .nest("/sync_commands", admin_only_crud(SyncCommand::router(db)))
        .nest("/sync_events", admin_only_crud(SyncEvent::router(db)))
        .nest("/pairing_plans", admin_only_crud(PairingPlan::router(db)))
        // Project-scoped API tokens may only mutate project-bound entities within their project
        // (fails closed on the global catalog). No-op for Keycloak users and unscoped tokens.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            enforce_scope_on_crud,
        ))
        // Read mirror of the above: confine list/get reads for project-bound entities to the
        // scoped token's project (CrudCrate `ScopeCondition`). No-op for unscoped callers and for
        // global/operational entities. Disjoint from the write guard above (it handles mutations).
        .layer(middleware::from_fn(inject_read_scope))
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
            get(crate::routes::private::sensors::calibrations::window::get_calibration_window),
        )
        .layer(middleware::from_fn(require_read_data))
        .with_state(state.clone());

    let stream_write_routes = Router::new()
        .route("/streams/register", post(stream_views::register_stream))
        .route("/streams/retag", post(stream_views::retag_streams))
        .route("/streams/{id}/import", post(stream_views::import_stream))
        .route("/streams/{id}/pair", post(stream_views::pair_stream))
        .route("/streams/{id}/unpair", post(stream_views::unpair_stream))
        .layer(middleware::from_fn(deny_scoped_token))
        // Stream registration/pairing is an Administrator action for humans; the write_metadata
        // token bit is preserved so sync-service session tokens keep registering streams.
        .layer(middleware::from_fn(require_admin_or_token_write_metadata))
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
        .route(
            "/sensors/retag_frequency",
            post(crate::routes::private::sensors::retag::retag_frequency),
        )
        .route("/actions/swap", post(sensor_adopt::swap_sensors))
        .layer(middleware::from_fn(deny_scoped_token))
        // Deploying/swapping a sensor at a slot is sensor movement: MANAGER (write_metadata token).
        .layer(middleware::from_fn(require_manage_sensors))
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
        .route("/actions/reconcile_alarms", post(actions::reconcile_alarms))
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
        .route("/alarms/thresholds", get(alarm_views::get_thresholds))
        .route("/events", get(crate::routes::private::events::event_stream))
        .route(
            "/reprocessing_jobs/{id}/logs",
            get(crate::routes::private::reprocessing_jobs::routes::get_job_logs),
        )
        .route("/tools", get(tools::list_tools))
        .route("/tools/{tool_name}/calculate", post(tools::calculate_tool))
        .layer(middleware::from_fn(require_read_data))
        .with_state(state.clone());

    let metadata_read_routes = Router::new()
        .route("/search", get(search::search))
        .route("/version", get(crate::routes::version::get_version))
        .route("/actions/backfill_candidates", get(actions::backfill_candidates))
        .route("/actions/calibration_candidates", get(actions::calibration_candidates))
        .route(
            "/schedules",
            get(crate::routes::private::reprocessing_jobs::schedule_routes::list_schedules),
        )
        .route(
            "/schedules/{job_name}",
            get(crate::routes::private::reprocessing_jobs::schedule_routes::get_schedule),
        )
        .route(
            "/schedules/{job_name}/audit",
            get(crate::routes::private::reprocessing_jobs::schedule_routes::get_schedule_audit),
        )
        .layer(middleware::from_fn(require_read_metadata))
        .with_state(state.clone());

    // Operator actions previously on /api/admin/: calibration recalc, sensor reprocess, derived
    // recompute, merges, job rerun/cancel, schedule control. MANAGER for humans (operators run
    // these); the write_metadata token bit is preserved for automation scripts. Scoped tokens are
    // denied (a logger key has no reason to trigger a global reprocess/merge).
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
        .route(
            "/reprocessing_jobs/{id}/rerun",
            post(crate::routes::private::reprocessing_jobs::routes::rerun_job),
        )
        .route(
            "/reprocessing_jobs/{id}/cancel",
            post(crate::routes::private::reprocessing_jobs::routes::cancel_job),
        )
        .route(
            "/schedules/{job_name}",
            patch(crate::routes::private::reprocessing_jobs::schedule_routes::update_schedule),
        )
        .route(
            "/schedules/{job_name}/run_now",
            post(crate::routes::private::reprocessing_jobs::schedule_routes::run_now),
        )
        .layer(RequestBodyLimitLayer::new(ACTION_BODY_LIMIT))
        .layer(middleware::from_fn(deny_scoped_token))
        .layer(middleware::from_fn(require_manage_sensors))
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
        // Human management of sync services is Administrator-only; write_metadata token preserved.
        .layer(middleware::from_fn(require_admin_or_token_write_metadata))
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

    // Telegram identity link-code minting. Admin-only — it grants a chat the linked user's role.
    let telegram_admin_routes = Router::new()
        .route(
            "/telegram_identities/link_code",
            post(crate::routes::private::notifications::views::generate_link_code),
        )
        .layer(middleware::from_fn(require_admin))
        .with_state(state.clone());

    // Admin notification oversight: per-channel health probe, one-off test send, subscriber roster.
    let notifications_admin_routes = {
        use crate::routes::private::notifications::{health, views as notif_views};
        Router::new()
            .route("/notifications/health", get(health::get_health))
            .route("/notifications/health/refresh", post(health::refresh_health))
            .route("/notifications/test-send", post(notif_views::test_send))
            .route("/notifications/subscribers", get(notif_views::list_subscribers))
            .layer(middleware::from_fn(require_admin))
            .with_state(state.clone())
    };

    // Self-service notification preferences. Any Keycloak user manages their OWN settings (the handler
    // binds to the caller's JWT sub; API tokens are refused in-handler since they have no user sub).
    let notifications_me_routes = {
        use crate::routes::private::notifications::me;
        Router::new()
            .route(
                "/notifications/me",
                get(me::get_my_notifications).patch(me::update_my_notifications),
            )
            .route("/notifications/me/subscriptions", put(me::set_my_subscriptions))
            .route("/notifications/me/link_code", post(me::mint_my_link_code))
            .route("/notifications/me/telegram", delete(me::unlink_my_telegram))
            .layer(middleware::from_fn(require_read_data))
            .with_state(state.clone())
    };

    // The caller's own identity, level, and project visibility. No extra capability gate — the
    // access gate in `service_auth_middleware` already guarantees a river role; the handler refuses
    // API tokens (no user sub) itself.
    let me_route = Router::new()
        .route("/me", get(crate::routes::private::me::get_me))
        .route("/me/sites", get(crate::routes::private::me::get_my_sites))
        .with_state(state.clone());

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
        .route(
            "/api_token_audit_logs/distinct/status_codes",
            get(crate::routes::private::api_tokens::audit_log::views::distinct_status_codes),
        )
        .layer(middleware::from_fn(require_admin))
        .with_state(state.clone());

    let mut router = Router::new()
        .merge(entity_router)
        .merge(token_admin_routes)
        .merge(telegram_admin_routes)
        .merge(notifications_admin_routes)
        .merge(notifications_me_routes)
        .merge(me_route)
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
