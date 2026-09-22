use axum::{
    Router,
    extract::{Request, State},
    middleware,
    response::Response,
    routing::{get, post},
};
use tower_http::limit::RequestBodyLimitLayer;
use utoipa_axum::router::OpenApiRouter;

use crate::common::AppState;
use crate::common::authz::{Capability, TokenAccess, TokenBit};
use crate::common::middleware::{
    bust_token_cache_on_mutation, deny_scoped_token, inject_project_scope, require_admin,
    require_admin_or_token_write_metadata, require_crud, require_enter_field_data,
    require_manage_sensors, require_read_data, require_read_metadata, require_write_data,
};
use crate::common::rate_limit::FallbackIpKeyExtractor;
use crate::routes::private::{
    alarms::models::{AlarmThreshold, alarm_event::AlarmEvent},
    annotations::Annotation,
    api_tokens::ApiToken,
    api_tokens::audit_log::ApiTokenAuditLog,
    change_audit::models::ChangeAudit,
    collection_events::CollectionEvent,
    constants::Constant,
    data_streams::DataStream,
    data_streams::models::receipts::IngestReceipt,
    data_streams::pairing_plans::PairingPlan,
    derived_parameters::models::definition::CalculationFormula,
    derived_parameters::models::shared_step::CalculationSharedStep,
    derived_parameters::models::source::DerivedParameterSource,
    meteoswiss::models::subscription::MeteoswissSubscription,
    notes::Note,
    notifications::{NotificationLog, NotificationMute, NotificationState, NotificationSubscriber},
    parameter_groups::group_model::ParameterGroup,
    parameter_groups::member_model::ParameterGroupMember,
    parameters::Parameter,
    projects::subprojects::Subproject,
    readings::decision_model::ReadingDecision,
    readings::models::Reading,
    readings::models::change_proposal::ReadingChangeProposal,
    readings::samples::Sample,
    reprocessing_jobs::ReprocessingJob,
    reprocessing_jobs::models::job_log::ReprocessingJobLog,
    reprocessing_jobs::models::schedule::Schedule,
    sensor_calibrations::SensorCalibration,
    sensor_deployments::SensorDeployment,
    sensors::Sensor,
    site_parameters::SiteParameter,
    standard_curves::StandardCurve,
    sync::hold_model::ReplicateAuditHold,
    sync::models::commands::SyncCommand,
    sync::models::credentials::SyncServiceCredential,
    sync::models::events::SyncEvent,
    sync::models::services::SyncService,
    tools::models::run::ToolRun,
};

pub(crate) const ACTION_BODY_LIMIT: usize = 1024 * 1024; // 1 MB, preserved from the former admin tier
pub(crate) const DATA_BODY_LIMIT: usize = 10 * 1024 * 1024; // 10 MB, bulk ingestion
pub(crate) const IMPORT_BODY_LIMIT: usize = 50 * 1024 * 1024; // 50 MB, CSV import

/// Clear the public API config cache after a successful mutating request.
///
/// Layered onto the projects/sites/site_parameters CRUD routers, the entities the
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
/// Returns the router and the OpenAPI document the entity half of it generates: every CrudCrate
/// route is described by the derive, and dropping the document is what left ~240 generated
/// operations out of `/docs`.
pub fn api_router(state: &AppState) -> (Router<()>, utoipa::openapi::OpenApi) {
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
    // Global catalog (parameters, derived definitions/sources, alarm thresholds,
    // notification mutes): MANAGER members may write.
    let catalog_crud = |r: OpenApiRouter| -> OpenApiRouter {
        r.layer(middleware::from_fn(require_crud(
            Capability::ReadMetadata,
            Capability::WriteCatalog,
            TokenAccess::Same,
        )))
    };
    // The lab's own catalog: the global parameter list and the instrument inventory. A manager
    // writes both (Q24: expected physical ranges on a parameter, manufacturer ranges and
    // calibration on an instrument, which is the lab's work and not an administrative act). The
    // write_metadata token bit is kept rather than `TokenAccess::Same`, because sync-service
    // session tokens mint sensors as they register streams.
    let catalog_inventory_crud = |r: OpenApiRouter| -> OpenApiRouter {
        r.layer(middleware::from_fn(require_crud(
            Capability::ReadMetadata,
            Capability::WriteCatalog,
            TokenAccess::Bit(TokenBit::WriteMetadata),
        )))
    };
    // Admin-managed inventory/system entities (data streams, reprocessing jobs): human writes are
    // Administrator-only, but the historical write_metadata token bit is preserved so sync-service
    // session tokens (which register streams and auto-create sensors) keep working.
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
        // entire surface, including LIST/GET, behind require_admin so leaked tokens
        // can't enumerate credentials. See plan: defense in depth.
        r.layer(middleware::from_fn(require_admin))
    };
    // Clear the public API config cache whenever a project/site/site_parameter is
    // created, updated, or deleted. Coarse and best-effort, see the middleware doc.
    let invalidate_public_config = |r: OpenApiRouter| -> OpenApiRouter {
        r.layer(middleware::from_fn_with_state(
            state.clone(),
            invalidate_public_config_on_mutation,
        ))
    };

    let (entity_router, entity_api): (Router<()>, utoipa::openapi::OpenApi) = OpenApiRouter::new()
        .nest(
            "/projects",
            invalidate_public_config(crate::routes::private::projects::views::service_router(
                state,
            )),
        )
        .nest(
            "/sites",
            invalidate_public_config(crate::routes::private::sites::views::service_router(state)),
        )
        .nest("/parameters", catalog_inventory_crud(Parameter::router(db)))
        .nest(
            "/site_parameters",
            invalidate_public_config(catalog_crud(SiteParameter::router(db))),
        )
        .nest("/sensors", catalog_inventory_crud(Sensor::router(db)))
        .nest(
            "/sensor_calibrations",
            sensor_crud(SensorCalibration::router(db)),
        )
        .nest(
            "/sensor_deployments",
            sensor_crud(SensorDeployment::router(db)),
        )
        // A schedule's rows are a projection of the job registry, so the entity mounts read and
        // update only; `run_now` and the edit trail stay their own routes beside it.
        .nest("/schedules", sensor_crud(Schedule::router(db)))
        // The review queue as rows, read-only: a hold is raised by the path that detects it and
        // decided through the named transitions under `/sync/replicate_audit_holds`.
        .nest(
            "/replicate_audit_holds",
            sensor_crud(ReplicateAuditHold::router(db)),
        )
        // The proposed corrections, read-only for the same reason: a proposal is raised by the
        // windowed ingest and decided through `/sync/change_proposals/decide`.
        .nest(
            "/reading_change_proposals",
            sensor_crud(ReadingChangeProposal::router(db)),
        )
        // Field metadata, not sensor movement: a standard curve affects only the grabs an operator
        // enters against it, so the person entering the plate's readings adds its curve in the same
        // sitting. `sensor_crud` (MANAGER) is the alternative, and would make them wait on a manager.
        .nest("/standard_curves", field_crud(StandardCurve::router(db)))
        .nest(
            "/derived_parameters",
            admin_write_crud(CalculationFormula::router(db)),
        )
        .nest(
            "/derived_parameter_sources",
            admin_write_crud(DerivedParameterSource::router(db)),
        )
        // One calculation's declaration that it reads a step owned by no calculation (Q156). The
        // list filtered by `formula_id` is what a step's own page reads to name its dependents.
        .nest(
            "/calculation_shared_steps",
            admin_write_crud(CalculationSharedStep::router(db)),
        )
        .nest(
            "/parameter_groups",
            admin_write_crud(ParameterGroup::router(db)),
        )
        .nest(
            "/parameter_group_members",
            admin_write_crud(ParameterGroupMember::router(db)),
        )
        .nest(
            "/alarm_thresholds",
            catalog_crud(AlarmThreshold::router(db)),
        )
        // Read only: an episode is opened by the sweeper and closed by a reading returning to
        // range, never by a client.
        .nest("/alarm_events", field_data_crud(AlarmEvent::router(db)))
        .nest(
            "/tokens",
            admin_only_crud(ApiToken::router(db)).layer(middleware::from_fn_with_state(
                state.clone(),
                bust_token_cache_on_mutation,
            )),
        )
        .nest(
            "/sync_service_credentials",
            admin_only_crud(SyncServiceCredential::router(db)),
        )
        // Read-only forensic audit trail of API-token use. Admin-only (no token can read it).
        .nest(
            "/api_token_audit_logs",
            admin_only_crud(ApiTokenAuditLog::router(db)),
        )
        // The append-only entity trail, across every subject. `GET /change_audit` answers the same
        // question for one subject.
        .nest("/change_audit_entries", field_crud(ChangeAudit::router(db)))
        // One row per calculation run, minted only by `/tools/{name}/calculate` and never edited,
        // which is why it mounts read-only. The provenance panel reads a saved reading's run here.
        .nest("/tool_runs", field_data_crud(ToolRun::router(db)))
        // The append-only curation ledger, read-only: every writer is a curation path with
        // its own rules. `GET /readings/decisions` is one reading's history.
        .nest(
            "/reading_decisions",
            field_data_crud(ReadingDecision::router(db)),
        )
        .nest("/data_streams", admin_write_crud(DataStream::router(db)))
        // One row per committed windowed ingest pass, read-only: the ingest writes them and the
        // janitor's age prune is the only delete. `GET /streams/{id}/receipts` is one stream's.
        .nest("/ingest_receipts", field_crud(IngestReceipt::router(db)))
        .nest(
            "/subprojects",
            invalidate_public_config(field_crud(Subproject::router(db))),
        )
        .nest("/notes", field_crud(Note::router(db)))
        .nest(
            "/notification_mutes",
            catalog_crud(NotificationMute::router(db)),
        )
        .nest(
            "/notification_logs",
            admin_only_crud(NotificationLog::router(db)),
        )
        .nest(
            "/notification_states",
            admin_only_crud(NotificationState::router(db)),
        )
        // The roster, `routes(read)`: a person's own `/notifications/me` is the only writer, and
        // the device count beside the switch is attached by the entity's own hook.
        .nest(
            "/notification_subscribers",
            admin_only_crud(NotificationSubscriber::router(db)),
        )
        .nest("/annotations", field_data_crud(Annotation::router(db)))
        // Q173: a constant feeds every formula that names it, so its creation and modification sit
        // with the calculations rather than with the catalog rows a manager edits.
        .nest("/constants", admin_write_crud(Constant::router(db)))
        .nest(
            "/meteoswiss_subscriptions",
            catalog_crud(MeteoswissSubscription::router(db)),
        )
        .nest("/samples", field_data_crud(Sample::router(db)))
        // The readings themselves, read-only: every write path resolves attribution from the
        // pairing and builds provenance server-side, and a change goes through the curation
        // routes, which append to `reading_decisions` first.
        .nest("/readings", field_data_crud(Reading::router(db)))
        .nest(
            "/collection_events",
            field_data_crud(CollectionEvent::router(db)),
        )
        .nest(
            "/reprocessing_jobs",
            admin_write_crud(ReprocessingJob::router(db)),
        )
        // Append-only, written only by a running job, so the generated read routes are the whole
        // surface. `GET /reprocessing_jobs/{id}/logs` is one job's timeline, tailed by `seq`.
        .nest(
            "/reprocessing_job_logs",
            admin_write_crud(ReprocessingJobLog::router(db)),
        )
        .nest("/sync_services", admin_only_crud(SyncService::router(db)))
        .nest("/sync_commands", admin_only_crud(SyncCommand::router(db)))
        .nest("/sync_events", admin_only_crud(SyncEvent::router(db)))
        .nest("/pairing_plans", admin_only_crud(PairingPlan::router(db)))
        // Confine every CRUD read and write for a project-bound entity to the caller's projects
        // (one CrudCrate `ScopeCondition`), and refuse a project-scoped token the entities that
        // have no project dimension. No-op for unscoped callers.
        .layer(middleware::from_fn(inject_project_scope))
        .split_for_parts();

    use crate::routes::private::{
        alarms::views as alarm_views, api_tokens::views as access_views,
        data_streams::views as stream_views, derived_parameters::views as derived_views,
        parameters::views as parameter_views, projects::views as project_views,
        readings::status_events::views as status_events_batch, readings::views as readings_views,
        search, sensor_calibrations::views as calibration_views,
        sensor_deployments::views as deployment_views, sensors::views as sensor_views,
        site_parameters::views as site_parameter_views, sync::views as sync_views, tools,
    };

    let stream_read_routes = stream_views::read_routes().with_state(state.clone());
    let stream_write_routes = stream_views::write_routes().with_state(state.clone());

    let sensor_view_read_routes =
        crate::routes::private::sensors::views::read_routes().with_state(state.clone());

    // What a sync service enrols through: one surface across five prefixes, held together by one
    // gate rather than by a component. It stays registered here for that reason, and is not any
    // one component's to take (Q143, C273).
    //
    // An Administrator action for humans; the `write_metadata` token bit is preserved so a
    // sync-service session token keeps registering what it discovers.
    let registration_routes = Router::new()
        .route("/streams/register", post(stream_views::register_stream))
        .route(
            "/standard_curves/register",
            post(crate::routes::private::standard_curves::views::register_standard_curve),
        )
        .route(
            "/sensors/proposals",
            post(crate::routes::private::sensors::views::propose_instruments),
        )
        .route(
            "/notes/register",
            post(crate::routes::private::notes::views::register_notes),
        )
        .route(
            "/annotations/register",
            post(crate::routes::private::annotations::views::register_annotations),
        )
        .layer(middleware::from_fn(deny_scoped_token))
        .layer(middleware::from_fn(require_admin_or_token_write_metadata))
        .with_state(state.clone());

    use crate::routes::private::sensors::views as sensor_adopt;
    let sensor_adopt_read = sensor_adopt::adopt_read_routes().with_state(state.clone());
    let sensor_adopt_write = sensor_adopt::adopt_write_routes().with_state(state.clone());

    // Instrument movement spelled as an action. Same gate as the component's own adopt routes, but
    // these are `/actions/*`, which stays registered centrally (Q143).
    let sensor_action_routes = Router::new()
        .route("/actions/swap", post(sensor_adopt::swap_sensors))
        // Rolling a deployment back undoes one, so it takes the capability deleting a deployment
        // takes rather than the weaker write_data the other operator actions carry.
        .route(
            "/actions/rollback_deployment",
            post(deployment_views::rollback_deployment),
        )
        .layer(middleware::from_fn(deny_scoped_token))
        .layer(middleware::from_fn(require_manage_sensors))
        .with_state(state.clone());

    // Data push paths. Each handler self-enforces project scope (a scoped token may only write
    // within its project), so these stay reachable by per-client logger keys.
    let data_push_routes = Router::new()
        .route("/ingest", post(readings_views::ingest_readings))
        .route(
            "/ingest/status_events",
            post(readings_views::ingest_status_events),
        )
        .route(
            "/status_events/batch",
            post(status_events_batch::insert_batch_status_events),
        )
        // The enforced cap, and the extractor's own limit above it: `/readings/import_csv` was
        // what raised the second one, and it stays because the push routes were reading under it
        // before that route moved to `readings/views.rs`.
        .layer(RequestBodyLimitLayer::new(DATA_BODY_LIMIT))
        .layer(axum::extract::DefaultBodyLimit::max(IMPORT_BODY_LIMIT))
        .route(
            "/collection_events/{id}/preview",
            post(crate::routes::private::collection_events::views::preview_collection_event),
        )
        .route(
            "/collection_events/{id}/recompute",
            post(crate::routes::private::collection_events::views::recompute_collection_event),
        )
        .route(
            "/actions/event_audit",
            post(crate::routes::private::collection_events::views::run_event_audit),
        )
        .route(
            "/actions/event_recompute",
            post(crate::routes::private::collection_events::views::run_event_recompute),
        )
        .layer(middleware::from_fn(require_write_data))
        .with_state(state.clone());

    // Entering a field measurement, and opening the field day it is entered at. An intern reaches
    // these and nothing else that writes: the save lands unverified and is refused a replace
    // (Q21, M44), and a visit they open is itself unverified until a manager rules on it (Q177).
    let field_entry_routes = Router::new()
        .route("/grab_samples", post(readings_views::insert_grab_samples))
        .route(
            "/collection_events/stage",
            post(crate::routes::private::collection_events::views::stage_collection_event),
        )
        .route(
            "/collection_events/stage_many",
            post(crate::routes::private::collection_events::views::stage_collection_events),
        )
        .layer(middleware::from_fn(require_enter_field_data))
        .with_state(state.clone());

    // Operator / global data actions that span projects or have no per-project target. Denied to
    // project-scoped tokens (a logger key has no reason to trigger a global reprocess/refresh).
    let data_action_routes = Router::new()
        .route(
            "/actions/compute_derived",
            post(derived_views::compute_derived),
        )
        .route("/actions/reprocess_all", post(sensor_views::reprocess_all))
        .route(
            "/actions/rebuild_alarm_events",
            post(alarm_views::rebuild_alarm_events),
        )
        .route(
            "/actions/reconcile_alarms",
            post(alarm_views::reconcile_alarms),
        )
        .route(
            "/actions/backfill_attribution",
            post(deployment_views::backfill_attribution),
        )
        .route(
            "/actions/backfill_calibrations",
            post(calibration_views::backfill_calibrations),
        )
        .layer(middleware::from_fn(deny_scoped_token))
        .layer(middleware::from_fn(require_write_data))
        .with_state(state.clone());

    let alarm_read_routes = alarm_views::read_routes().with_state(state.clone());
    let alarm_write_routes = alarm_views::write_routes().with_state(state.clone());

    let data_read_routes = Router::new()
        .route(
            "/actions/preview_derived",
            post(derived_views::preview_derived),
        )
        .route(
            "/events",
            get(crate::routes::private::events::views::event_stream),
        )
        .route(
            "/reprocessing_jobs/{id}/logs",
            get(crate::routes::private::reprocessing_jobs::views::get_job_logs),
        )
        .route("/tools", get(tools::views::list_tools))
        // What a step feeds: a step mints no parameter, so the closure above cannot answer for it.
        .route(
            "/derived_parameters/{id}/dependents",
            get(derived_views::step_dependents),
        )
        .route(
            "/calculations/closure",
            get(crate::routes::private::tools::views::get_calculation_closure),
        )
        .route(
            "/calculations/health",
            get(crate::routes::private::tools::views::get_calculation_health),
        )
        .route(
            "/tools/{tool_name}/calculate",
            post(tools::views::calculate_tool),
        )
        .route(
            "/tools/{tool_name}/preview",
            post(tools::views::preview_tool),
        )
        .route(
            "/tool_runs/{id}/reload",
            get(crate::routes::private::readings::views::reload_run),
        )
        .route(
            "/tool_runs/{id}/trace",
            get(crate::routes::private::tools::views::trace_run),
        )
        .route(
            "/sites/{id}/visits",
            get(crate::routes::private::collection_events::views::list_site_visits),
        )
        .route(
            "/visits",
            get(crate::routes::private::collection_events::views::list_visits),
        )
        .route(
            "/collection_events/{id}/detail",
            get(crate::routes::private::collection_events::views::get_event_detail),
        )
        // The rows carry a reading's stored value, so this is data rather than metadata.
        .route(
            "/actions/curation_drift",
            get(readings_views::curation_drift),
        )
        .layer(middleware::from_fn(require_read_data))
        .with_state(state.clone());

    let metadata_read_routes = Router::new()
        .route("/search", get(search::views::search))
        .route("/version", get(crate::routes::version::get_version))
        .route(
            "/actions/backfill_candidates",
            get(deployment_views::backfill_candidates),
        )
        .route(
            "/actions/calibration_candidates",
            get(calibration_views::calibration_candidates),
        )
        .route(
            "/parameter_groups/{id}/definition",
            get(crate::routes::private::parameter_groups::views::group_definition),
        )
        .route(
            "/schedules/runnable",
            get(crate::routes::private::reprocessing_jobs::views::list_runnable),
        )
        .route(
            "/schedules/{job_name}/audit",
            get(crate::routes::private::reprocessing_jobs::views::get_schedule_audit),
        )
        .route(
            "/change_audit",
            get(crate::routes::private::change_audit::views::list_change_audit),
        )
        .route(
            "/meteoswiss/stations",
            get(crate::routes::private::meteoswiss::views::list_stations),
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
            post(calibration_views::recalculate_calibration),
        )
        .route("/actions/reprocess", post(sensor_views::reprocess_sensor))
        .route(
            "/sensor_calibrations/{id}/retire",
            post(crate::routes::private::sensor_calibrations::views::retire_calibration),
        )
        .route(
            "/sensor_calibrations/{id}/unretire",
            post(crate::routes::private::sensor_calibrations::views::unretire_calibration),
        )
        .route(
            "/standard_curves/{id}/retire",
            post(crate::routes::private::standard_curves::views::retire_standard_curve),
        )
        .route(
            "/standard_curves/{id}/unretire",
            post(crate::routes::private::standard_curves::views::unretire_standard_curve),
        )
        .route(
            "/actions/derived_parameters/{id}/recompute",
            post(derived_views::recompute_derived),
        )
        .route(
            "/actions/invalidate_public_config/{code}",
            post(project_views::invalidate_public_config),
        )
        .route(
            "/reprocessing_jobs/{id}/rerun",
            post(crate::routes::private::reprocessing_jobs::views::rerun_job),
        )
        .route(
            "/reprocessing_jobs/{id}/cancel",
            post(crate::routes::private::reprocessing_jobs::views::cancel_job),
        )
        .route(
            "/schedules/{job_name}/run_now",
            post(crate::routes::private::reprocessing_jobs::views::run_now),
        )
        // A declaration change recomputes the slot's stored samples, the same act as the audit
        // resolution's slot scope, so it carries the same MANAGER gate rather than catalog CRUD.
        // Applying a group is what declares a site's calculations (Q98, narrowed by Q193: the
        // inputs are the declaration), so it carries the same MANAGER gate the other slot-shaping
        // actions do.
        .route(
            "/sites/{site_id}/parameter_groups",
            post(crate::routes::private::site_parameters::views::apply_group),
        )
        .layer(RequestBodyLimitLayer::new(ACTION_BODY_LIMIT))
        .layer(middleware::from_fn(deny_scoped_token))
        .layer(middleware::from_fn(require_manage_sensors))
        .with_state(state.clone());

    // A merge destroys rows across projects: the catalog merge hard-deletes a `parameters` row and
    // the slot merge moves and deletes readings at any site named by id. That reach, not the
    // catalog level, is why both hold the Administrator gate where the parameter and instrument
    // CRUD surfaces sit at MANAGER (Q24) and the other operator actions carry the MANAGER gate.
    // The write_metadata token bit is preserved for automation; scoped tokens are still denied, a
    // per-project key has no business destroying another project's data.
    let catalog_merge_routes = Router::new()
        .route(
            "/actions/merge_parameters",
            post(parameter_views::merge_parameters_handler),
        )
        .route(
            "/actions/merge_site_parameters",
            post(site_parameter_views::merge_site_parameters_handler),
        )
        .layer(RequestBodyLimitLayer::new(ACTION_BODY_LIMIT))
        .layer(middleware::from_fn(deny_scoped_token))
        .layer(middleware::from_fn(require_admin_or_token_write_metadata))
        .with_state(state.clone());

    // Sync admin views split by required permission. Credential creation/revoke is
    // require_admin because it mints full-permission sync session tokens, a token
    // with write_metadata must not be able to bootstrap a more privileged token.
    // Each sync group carries its own layers in `sync/views.rs` (Q143), so these only mount them.
    let sync_admin_read = Router::new()
        .nest("/sync", sync_views::read_routes())
        .with_state(state.clone());

    let sync_admin_write = Router::new()
        .nest("/sync", sync_views::write_routes())
        .with_state(state.clone());

    let sync_admin_manage = Router::new()
        .nest("/sync", sync_views::manage_routes())
        .with_state(state.clone());

    let sync_admin_admin = Router::new()
        .nest("/sync", sync_views::admin_routes())
        .with_state(state.clone());

    // Keycloak user management proxy, mounted only when AppState holds admin client credentials.
    // The component carries its own `require_admin` layer.
    let user_routes = state
        .keycloak_admin
        .as_ref()
        .map(|_| access_views::realm_routes().with_state(state.clone()));

    let tool_script_routes =
        crate::routes::private::tools::views::script_routes().with_state(state.clone());

    let notifications_admin_routes =
        crate::routes::private::notifications::views::oversight_routes().with_state(state.clone());

    let notifications_me_routes =
        crate::routes::private::notifications::views::subscriber_routes().with_state(state.clone());

    // The caller's own identity, level, and project visibility. No extra capability gate, the
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
            get(crate::routes::private::api_tokens::views::distinct_status_codes),
        )
        .layer(middleware::from_fn(require_admin))
        .with_state(state.clone());

    let mut router = Router::new()
        .merge(entity_router)
        .merge(token_admin_routes)
        .merge(tool_script_routes)
        .merge(notifications_admin_routes)
        .merge(notifications_me_routes)
        .merge(me_route)
        .merge(stream_read_routes)
        .merge(sensor_view_read_routes)
        .merge(stream_write_routes)
        .merge(registration_routes)
        .merge(sensor_adopt_read)
        .merge(sensor_adopt_write)
        .merge(sensor_action_routes)
        .merge(metadata_read_routes)
        .merge(readings_views::readings_read_routes(state))
        .merge(readings_views::readings_write_routes(state))
        .merge(readings_views::readings_admin_routes(state))
        .merge(data_push_routes)
        .merge(field_entry_routes)
        .merge(data_action_routes)
        .merge(alarm_read_routes)
        .merge(alarm_write_routes)
        .merge(data_read_routes)
        .merge(operator_action_routes)
        .merge(catalog_merge_routes)
        .merge(sync_admin_read)
        .merge(sync_admin_write)
        .merge(sync_admin_manage)
        .merge(sync_admin_admin);

    if let Some(routes) = user_routes {
        router = router.merge(routes);
    }

    // A CRUD batch runs the single-row hooks once per row, so the global alarm reconcile they ask
    // for is owed once per request rather than once per row.
    let router = router.layer(middleware::from_fn_with_state(
        state.clone(),
        crate::routes::private::alarms::views::coalesce_reconcile,
    ));

    (router, under_api(entity_api))
}

/// The entity document's paths as the server serves them. The derive describes each route
/// relative to the router it generated, and this whole router is mounted at `/api`, so a key in
/// the merged document is only a URL once it carries that prefix.
fn under_api(mut api: utoipa::openapi::OpenApi) -> utoipa::openapi::OpenApi {
    api.paths.paths = api
        .paths
        .paths
        .into_iter()
        .map(|(path, item)| (format!("/api{path}"), item))
        .collect();
    api
}

pub fn sync_control_router(state: &AppState) -> Router<AppState> {
    use std::sync::Arc;
    use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};

    let enroll_limiter = GovernorConfigBuilder::default()
        .key_extractor(FallbackIpKeyExtractor::new(
            &state.config.trusted_proxy_cidrs,
        ))
        .per_second(3)
        .burst_size(10)
        .finish()
        .expect("Failed to create enroll rate limiter");

    // Only enrollment is throttled (credential brute force). The session-token routes carry the
    // services' own observability records and four services booting at once behind one ingress
    // IP were observed to 429 a cycle-record write, so they bypass the limiter entirely.
    let enroll =
        crate::routes::private::sync::views::control_enroll_routes().layer(GovernorLayer {
            config: Arc::new(enroll_limiter),
        });
    let session = crate::routes::private::sync::views::control_session_routes();

    Router::new().nest("/sync", enroll.merge(session))
}

#[cfg(test)]
#[path = "tests/route_guards.rs"]
mod route_guards;
