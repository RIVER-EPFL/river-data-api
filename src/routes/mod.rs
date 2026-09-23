pub mod config;
pub mod private;
pub mod public;
pub mod public_api;
pub mod service;
pub mod version;

pub use crate::common::cache;

use axum::{Router, http::StatusCode, middleware, routing::get};
use sea_orm::{
    Condition, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter, Statement,
    sea_query::Expr,
};
use std::sync::Arc;
use std::time::Duration;
use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};
use uuid::Uuid;

use crate::common::rate_limit::FallbackIpKeyExtractor;
use tower_http::{
    compression::CompressionLayer,
    cors::CorsLayer,
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    timeout::TimeoutLayer,
    trace::{DefaultMakeSpan, TraceLayer},
};
use tracing::Level;
use utoipa::OpenApi;
use utoipa_scalar::{Scalar, Servable};

use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::{projects as projects_entity, sites as sites_entity};

/// Liveness probe, returns 200 if the process is running.
#[utoipa::path(
    get,
    path = "/healthz",
    responses(
        (status = 200, description = "Service is alive"),
    ),
    tag = "health"
)]
async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// Readiness probe, returns 200 only if the database is reachable.
#[utoipa::path(
    get,
    path = "/readyz",
    responses(
        (status = 200, description = "Service is ready"),
        (status = 503, description = "Database unreachable"),
    ),
    tag = "health"
)]
async fn readyz(axum::extract::State(state): axum::extract::State<AppState>) -> StatusCode {
    let result = state
        .db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT 1".to_string(),
        ))
        .await;
    match result {
        Ok(Some(_)) => StatusCode::OK,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// Resolve any entity by UUID or case-insensitive name lookup.
async fn resolve_by_id_or_name<E>(
    db: &DatabaseConnection,
    id_or_name: &str,
    label: &str,
) -> AppResult<E::Model>
where
    E: EntityTrait,
    E::Model: Send,
    <<E as EntityTrait>::PrimaryKey as sea_orm::PrimaryKeyTrait>::ValueType: From<Uuid>,
{
    if let Ok(uuid) = id_or_name.parse::<Uuid>() {
        return E::find_by_id(uuid)
            .one(db)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("{label} not found")));
    }
    E::find()
        .filter(Condition::all().add(Expr::cust_with_values(
            "LOWER(name) = LOWER($1)",
            [id_or_name],
        )))
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("{label} not found")))
}

pub async fn resolve_project(
    db: &DatabaseConnection,
    id_or_name: &str,
) -> AppResult<projects_entity::Model> {
    resolve_by_id_or_name::<projects_entity::Entity>(db, id_or_name, "Project").await
}

pub async fn resolve_site(
    db: &DatabaseConnection,
    id_or_name: &str,
) -> AppResult<sites_entity::Model> {
    resolve_by_id_or_name::<sites_entity::Entity>(db, id_or_name, "Site").await
}

/// Resolve a site by UUID or name, fetching the related project in the same query.
/// Returns (site, Option<project>) to avoid a separate N+1 project lookup.
pub async fn resolve_site_with_project(
    db: &DatabaseConnection,
    id_or_name: &str,
) -> AppResult<(sites_entity::Model, Option<projects_entity::Model>)> {
    // Try UUID first
    if let Ok(uuid) = id_or_name.parse::<Uuid>() {
        return sites_entity::Entity::find_by_id(uuid)
            .find_also_related(projects_entity::Entity)
            .one(db)
            .await?
            .ok_or_else(|| AppError::NotFound("Site not found".to_string()));
    }

    // Fall back to case-insensitive name lookup using LOWER()
    sites_entity::Entity::find()
        .filter(Condition::all().add(Expr::cust_with_values(
            "LOWER(sites.name) = LOWER($1)",
            [id_or_name],
        )))
        .find_also_related(projects_entity::Entity)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Site not found".to_string()))
}

/// Validate that a required time range has end >= start.
pub fn validate_time_range(
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
) -> AppResult<()> {
    if end < start {
        return Err(AppError::BadRequest(
            "end time must not be before start time".to_string(),
        ));
    }
    Ok(())
}

/// Validate an optional time range (only checks if both are provided).
pub fn validate_optional_time_range(
    start: Option<chrono::DateTime<chrono::Utc>>,
    end: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<()> {
    if let (Some(s), Some(e)) = (start, end) {
        validate_time_range(s, e)?;
    }
    Ok(())
}

#[derive(OpenApi)]
#[openapi(
    paths(
        healthz,
        private::sensor_deployments::views::backfill_candidates,
        private::sensor_deployments::views::backfill_attribution,
        private::alarms::views::get_active_alarms,
        private::alarms::views::get_alarm_summary,
        private::api_tokens::views::distinct_status_codes,
        private::api_tokens::views::revoke_token,
        private::api_tokens::views::rotate_token,
        private::api_tokens::views::token_usage,
        private::notifications::views::get_my_notifications,
        private::notifications::views::list_channels,
        private::notifications::views::update_my_notifications,
        private::notifications::views::set_my_subscriptions,
        private::notifications::views::register_push_subscription,
        private::notifications::views::list_push_subscriptions,
        private::notifications::views::delete_push_subscription,
        private::notifications::views::test_push,
        private::notifications::views::schedule_ping,
        private::notifications::views::test_send,
        private::notifications::views::get_health,
        private::notifications::views::refresh_health,
        private::events::views::event_stream,
        private::change_audit::views::list_change_audit,
        private::reprocessing_jobs::views::list_runnable,
        private::meteoswiss::views::list_stations,
        private::me::get_me,
        private::me::get_my_sites,
        private::api_tokens::views::list_user_grants,
        private::api_tokens::views::set_user_grants,
        private::readings::views::decide_proposals,
        private::reprocessing_jobs::views::get_job_logs,
        private::reprocessing_jobs::views::rerun_job,
        private::reprocessing_jobs::views::cancel_job,
        private::reprocessing_jobs::views::run_now,
        private::reprocessing_jobs::views::get_schedule_audit,
        private::notifications::views::list_delivery_log,
        private::sites::views::get_site_export_summary,
        version::get_version,
        private::projects::views::list_project_sites,
        private::sites::views::list_site_parameters,
        private::sites::views::get_site_detail,
        private::sites::views::get_site_readings,
        private::sites::views::get_site_aggregates,
        private::sites::views::get_site_status_events,
        private::alarms::views::get_site_alarms,
        private::alarms::views::get_alarm_events,
        private::alarms::views::get_thresholds,
        private::alarms::views::acknowledge_alarm,
        private::alarms::views::unacknowledge_alarm,
        private::sites::views::get_site_annotations,
        private::search::views::search,
        private::readings::views::flag_readings,
        private::readings::views::unflag_readings,
        private::readings::views::flag_range,
        private::readings::views::unflag_range,
        private::readings::views::ingest_readings,
        private::readings::views::ingest_status_events,
        private::readings::views::insert_batch_readings,
        private::readings::views::insert_grab_samples,
        private::readings::views::seasonal_check,
        private::collection_events::views::stage_collection_event,
        private::collection_events::views::stage_collection_events,
        private::collection_events::views::preview_collection_event,
        private::collection_events::views::preview_unstaged_visit,
        private::collection_events::views::recompute_collection_event,
        private::collection_events::views::run_event_audit,
        private::collection_events::views::run_event_recompute,
        private::collection_events::views::list_site_visits,
        private::collection_events::views::list_visits,
        private::collection_events::views::get_event_detail,
        private::readings::views::get_reading_provenance,
        private::readings::views::get_reading_ledger,
        private::readings::views::sample_preview,
        private::readings::views::list_decisions,
        private::readings::views::replay_derived,
        private::sensor_calibrations::views::retire_calibration,
        private::sensor_calibrations::views::unretire_calibration,
        private::standard_curves::views::retire_standard_curve,
        private::standard_curves::views::unretire_standard_curve,
        private::readings::views::detach_output,
        private::readings::views::return_output,
        private::readings::views::inspect,
        private::readings::views::preview,
        private::readings::views::commit,
        private::readings::views::rollback,
        private::readings::views::rollback_edit_set,
        private::readings::views::reload_run,
        private::readings::status_events::views::insert_batch_status_events,
        private::data_streams::views::stream_stats,
        private::data_streams::views::stream_preview,
        private::data_streams::views::stream_receipts,
        private::data_streams::views::register_stream,
        private::sensors::views::propose_instruments,
        private::standard_curves::views::register_standard_curve,
        private::standard_curves::views::last_used_curve,
        private::sensors::views::get_instruments_overview,
        private::sensors::views::last_used_instruments,
        private::sensors::views::get_curve_usage,
        private::sensors::views::get_sensor_curve_usage,
        private::annotations::views::register_annotations,
        private::notes::views::register_notes,
        private::sync::views::list_holds,
        private::sync::views::release_brake,
        private::sync::views::dismiss_finding,
        private::sync::views::accept_identity,
        private::sync::views::accept_correction,
        private::sync::views::resolve_hold,
        private::sync::views::reopen_hold,
        private::sync::views::reject_preview,
        private::data_streams::views::retag_streams,
        private::data_streams::views::pair_stream,
        private::data_streams::views::unpair_stream,
        private::readings::views::import_csv,
        private::readings::views::import_csv_chunk,
        private::sensors::views::get_sensor_readings,
        private::sensors::views::get_sensor_deployment_bands,
        private::sensors::views::adopt_sensor,
        private::sensors::views::adopt_suggestions,
        private::sensors::views::swap_sensors,
        private::sensors::views::retag_frequency,
        private::sensor_calibrations::views::get_calibration_window,
        private::sites::views::get_site_sensor_identity,
        private::sites::views::get_site_replicates,
        private::sites::views::get_sensor_vs_grab,
        private::sites::views::get_site_statistics,
        private::tools::views::get_calculation_closure,
        private::tools::views::get_calculation_health,
        private::tools::views::list_tools,
        private::tools::views::calculate_tool,
        private::tools::views::preview_tool,
        private::tools::views::trace_run,
        private::tools::views::list_scripts,
        private::tools::views::get_script,
        private::tools::views::create_script,
        private::tools::views::update_script,
        private::tools::views::create_version,
        private::tools::views::draft_run,
        private::tools::views::draft_run_formulas,
        private::tools::views::save_formula_set,
        private::tools::views::inspect_script,
        private::tools::views::get_version,
        private::tools::views::validate_version,
        private::tools::views::activate_version,
        private::tools::views::list_activations,
        private::tools::views::list_version_usage,
        private::tools::views::list_version_ledger,
        private::derived_parameters::views::compute_derived,
        private::sensors::views::reprocess_sensor,
        private::sensors::views::reprocess_all,
        private::alarms::views::rebuild_alarm_events,
        private::alarms::views::reconcile_alarms,
        private::sensor_deployments::views::rollback_deployment,
        private::sensor_calibrations::views::calibration_candidates,
        private::readings::views::curation_drift,
        private::sensor_calibrations::views::backfill_calibrations,
        private::derived_parameters::views::preview_derived,
        private::sensor_calibrations::views::recalculate_calibration,
        private::derived_parameters::views::recompute_derived,
        private::derived_parameters::views::step_dependents,
        private::site_parameters::views::merge_site_parameters_handler,
        private::parameter_groups::views::group_definition,
        private::site_parameters::views::apply_group,
        private::site_parameters::views::apply_calculation,
        private::parameters::views::merge_parameters_handler,
        private::projects::views::invalidate_public_config,
        private::sync::views::create_pairing_plan,
        private::sync::views::list_pairing_plans,
        private::sync::views::get_pairing_plan,
        private::sync::views::update_pairing_plan,
        private::sync::views::apply_pairing_plan,
        private::sync::views::supersede_pairing_plan,
        private::sync::views::revert_pairing_plan,
        private::sync::views::unpaired_summary,
        private::sync::views::plan_site_metadata,
        private::sync::views::plan_instruments,
        private::api_tokens::views::list_users,
        private::api_tokens::views::search_users,
        private::api_tokens::views::get_user,
        private::api_tokens::views::update_user,
        private::api_tokens::views::delete_user,
        private::api_tokens::views::assign_roles,
        private::api_tokens::views::list_roles,
        private::sync::views::enroll,
        private::sync::views::heartbeat,
        private::sync::views::update_command,
        private::sync::views::create_sync_event,
        private::sync::views::update_sync_event,
        private::sync::views::issue_command,
        private::sync::views::create_credential,
        private::sync::views::revoke_credential,
        private::sync::views::revoke_service,
    ),
    components(
        schemas(
            private::notifications::models::DeliveryMessage,
            private::notifications::models::DeliveryRecipient,
            private::notifications::models::DeliveryCounts,
            private::projects::models::ProjectResponse,
            private::sites::models::SiteProjection,
            private::sites::models::SiteDetailResponse,
            private::sites::models::SiteRef,
            private::sites::models::ProjectRef,
            private::sites::models::ParameterResponse,
            // The three types a crudcrate `join` field is declared as. The joined rows travel as
            // the entity's own api_struct rather than its `*Response`, so these are what the
            // `$ref`s on `SiteParameterResponse.parameter`, `SensorResponse.deployments` and
            // `CalculationFormulaResponse.sources` name.
            private::parameters::Parameter,
            private::derived_parameters::models::source::DerivedParameterSource,
            private::sensor_deployments::SensorDeployment,
            private::sites::models::ReadingsResponse,
            private::sites::models::ParameterData,
            private::sites::models::OriginRef,
            private::sites::models::SampleStatOut,
            private::sites::models::ReplicateOut,
            private::sites::models::AggregatesResponse,
            private::sites::models::ParameterAggregateData,
            private::sites::models::StatusEventsResponse,
            private::annotations::Annotation,
            private::alarms::models::AlarmViolationsResponse,
            private::alarms::models::ParameterViolationData,
            private::alarms::models::AlarmEventResponse,
            private::alarms::models::AlarmEventsResponse,
            private::search::SearchResponse,
            private::search::SearchResults,
            private::search::SiteResult,
            private::search::SensorResult,
            private::search::ParameterResult,
            private::search::ProjectResult,
            private::readings::models::ReadingKey,
            private::readings::models::FlagReadingsRequest,
            private::readings::models::UnflagReadingsRequest,
            private::readings::models::FlagReadingsResponse,
            private::readings::models::FlagRangeRequest,
            private::readings::models::UnflagRangeRequest,
            private::readings::models::IngestReadingsRequest,
            private::readings::models::IngestReading,
            private::readings::models::IngestResponse,
            private::readings::models::IngestStatusEventsRequest,
            private::readings::models::IngestStatusEvent,
            private::readings::models::IngestStatusEventsResponse,
            private::readings::models::BatchReadingsRequest,
            private::readings::models::ReadingInput,
            private::readings::models::BatchReadingsResponse,
            private::readings::models::GrabSampleRequest,
            private::readings::models::SeasonalCheckRequest,
            private::readings::models::SeasonalCheckResponse,
            private::readings::models::SeasonalFinding,
            private::readings::models::SeasonalMethod,
            private::readings::models::SeasonalClassDescription,
            private::readings::models::SeasonalCheckValue,
            private::readings::models::SourceWindow,
            private::collection_events::EventAuditRequest,
            private::collection_events::EventRecomputeRequest,
            private::collection_events::EnqueuedJobResponse,
            private::collection_events::models::VisitsResponse,
            private::collection_events::models::VisitListRow,
            private::collection_events::models::VisitListRow,
            private::collection_events::models::VisitRow,
            private::collection_events::models::VisitCell,
            private::collection_events::models::ExpectedParameter,
            private::collection_events::models::EventDetailResponse,
            private::collection_events::models::EventCell,
            private::collection_events::models::CellSample,
            private::collection_events::models::CellReplicate,
            private::collection_events::models::CellFinding,
            private::readings::models::DecisionRow,
            private::sensor_calibrations::views::RetireRequest,
            private::sensor_calibrations::views::RetireResponse,
            private::sensor_calibrations::views::UnretireResponse,
            private::standard_curves::models::RetireCurveRequest,
            private::standard_curves::models::RetireCurveResponse,
            private::readings::models::Selection,
            private::readings::models::SelectionKey,
            private::readings::models::OutputSlotRequest,
            private::readings::models::OwnershipResponse,
            private::readings::models::Owner,
            private::readings::models::Kind,
            private::readings::models::Origin,
            private::readings::models::ProvenanceResponse,
            private::readings::models::ProvenanceRecord,
            private::readings::models::OriginInfo,
            private::readings::models::PinRef,
            private::readings::models::ReceiptSummary,
            private::readings::models::ReadingFacet,
            private::readings::models::CalibrationRef,
            private::readings::models::CurveRef,
            private::readings::models::ChainInfo,
            private::readings::models::SensorRef,
            private::readings::models::DeploymentRef,
            private::readings::models::EventRef,
            private::readings::models::ComputationInfo,
            private::readings::models::HoldRef,
            private::readings::models::GrabSampleReading,
            private::readings::models::GrabSampleResponse,
            private::readings::status_events::views::BatchStatusEventsRequest,
            private::readings::status_events::views::StatusEventInput,
            private::readings::status_events::views::BatchStatusEventsResponse,
            private::data_streams::models::StreamStatsResponse,
            private::data_streams::models::ReceiptsResponse,
            private::data_streams::models::ReceiptRow,
            private::data_streams::models::RegisterStreamRequest,
            river_data_core::models::ReplicateSpec,
            private::data_streams::models::ColumnAssignment,
            private::notes::models::RegisterNotesRequest,
            private::notes::models::RegisterNotesResponse,
            private::standard_curves::models::RegisterStandardCurveRequest,
            private::standard_curves::models::RegisterStandardCurveResponse,
            private::annotations::models::RegisterAnnotationsRequest,
            private::annotations::models::AnnotationItem,
            private::annotations::models::RegisterAnnotationsResponse,
            private::annotations::models::AnnotationOutcome,
            private::sync::models::GroupAudit,
            private::sync::models::HoldRow,
            private::sync::models::HoldExpected,
            private::sync::models::HoldComputed,
            private::sync::models::HoldDelta,
            private::sync::models::HoldValue,
            private::sync::models::UpdatePairingPlanRequest,
            private::sync::models::PlanEntryUpdate,
            private::sync::models::PlanCurveUpdate,
            private::sync::models::BulkAction,
            private::sync::service::BulkWhere,
            private::sync::models::ListHoldsResponse,
            private::sync::models::AcknowledgeResponse,
            private::sync::models::ResolveHoldRequest,
            private::sync::models::ResolveHoldResponse,
            private::sync::models::RejectPreview,
            private::sync::models::RejectPreviewOutput,
            private::data_streams::DataStream,
            private::data_streams::models::PairStreamRequest,
            private::data_streams::models::PairStreamResponse,
            private::data_streams::models::UnpairStreamResponse,
            private::readings::models::ImportCsvRequest,
            private::readings::models::ImportCsvResponse,
            private::readings::models::ImportCheck,
            private::readings::models::ScreenedCell,
            private::readings::models::OverlapDiff,
            private::readings::models::RowError,
            private::sensors::models::SensorReadingsResponse,
            private::sensors::models::SensorDeploymentBand,
            private::sensors::models::SensorDeploymentBandsResponse,
            private::sensors::models::AdoptRequest,
            private::sensors::models::AdoptResponse,
            private::sensors::models::AdoptSuggestion,
            private::sensors::models::SwapRequest,
            private::sensors::models::SwapResponse,
            private::sensor_calibrations::views::CalibrationWindowPoint,
            private::sensor_calibrations::views::CalibrationWindowResponse,
            private::sites::models::IdentityBand,
            private::sites::models::CalibrationMarker,
            private::sites::models::SensorIdentityResponse,
            private::sites::models::SensorVsGrabRow,
            private::sites::models::SensorVsGrabResponse,
            private::tools::models::ToolCalculation,
            private::tools::models::ToolResult,
            private::tools::models::RunTrace,
            private::tools::models::Manifest,
            private::tools::models::ManifestParam,
            private::tools::models::ParamWhen,
            private::tools::models::ParamCondition,
            private::tools::models::ManifestStructure,
            private::tools::models::ManifestField,
            private::tools::models::FieldFormula,
            private::tools::models::StructLayout,
            private::tools::models::RowLabels,
            private::tools::models::ManifestOutput,
            private::tools::models::ManifestCurve,
            private::tools::models::ToolOutput,
            private::tools::models::ResolvedParameter,
            private::tools::models::ResolvedBy,
            private::tools::models::ToolDescriptor,
            private::tools::models::ToolVersionRef,
            private::tools::models::script::ToolScript,
            private::tools::models::script::ToolScriptList,
            private::tools::models::CreateScriptRequest,
            private::tools::models::UpdateScriptRequest,
            private::tools::models::version::ToolScriptVersion,
            private::tools::models::version::ToolScriptVersionList,
            private::tools::models::CreateVersionRequest,
            private::tools::models::CreateVersionResponse,
            private::tools::models::DraftRunRequest,
            private::tools::models::DraftRunResponse,
            private::tools::models::DraftRunResults,
            private::tools::models::DraftRunFailure,
            private::tools::models::DraftRunFailureKind,
            private::tools::models::DraftFormula,
            private::tools::models::SavedFormula,
            private::tools::models::SaveFormulaSetRequest,
            private::tools::models::SaveFormulaSetResponse,
            private::tools::models::FormulaDraftRunRequest,
            private::tools::models::FormulaDraftRunResponse,
            private::tools::models::FormulaDraftRunResults,
            private::tools::models::InspectScriptRequest,
            private::tools::models::InspectScriptResponse,
            private::tools::models::ScriptInspection,
            private::tools::models::ParseError,
            private::tools::models::DynamicFlag,
            private::tools::models::ManifestReconciliation,
            private::tools::models::LintFinding,
            private::tools::models::CaseResult,
            private::tools::models::ValidateResponse,
            private::tools::models::ActivateRequest,
            private::tools::models::ActivateResponse,
            private::tools::models::ActivationRecord,
            private::tools::models::VersionUsage,
            private::tools::models::VersionLedgerRow,
            private::derived_parameters::views::ComputeDerivedRequest,
            private::sensors::views::ReprocessSensorRequest,
            private::derived_parameters::views::SiteTimestamps,
            private::sensor_deployments::views::RollbackDeploymentRequest,
            private::sensor_deployments::views::RollbackDeploymentResponse,
            private::sensors::views::ReprocessAllResponse,
            private::derived_parameters::views::PreviewDerivedRequest,
            private::derived_parameters::views::PreviewDerivedResponse,
            private::derived_parameters::views::PreviewSite,
            private::derived_parameters::views::SourceParameterSeries,
            private::derived_parameters::views::DerivedSeries,
            private::sensor_calibrations::views::CalibrationBackfillCandidate,
            private::sensor_calibrations::views::CalibrationBackfillCandidatesResponse,
            private::sensor_calibrations::views::OrphanedCorrection,
            private::sensor_calibrations::views::BackfillCalibrationsRequest,
            private::sensor_calibrations::views::BackfillCalibrationsResponse,
            private::site_parameters::service::MergeSiteParametersRequest,
            private::site_parameters::service::MergeSiteParametersResponse,
            private::parameters::service::MergeParametersRequest,
            private::parameters::service::MergeParametersResponse,
            private::api_tokens::models::AssignRolesRequest,
            private::api_tokens::models::KeycloakRole,
            river_data_core::models::EnrollRequest,
            river_data_core::models::EnrollResponse,
            river_data_core::models::HeartbeatRequest,
            river_data_core::models::HeartbeatResponse,
            river_data_core::models::PendingCommand,
            river_data_core::models::CommandUpdateRequest,
            private::sync::models::CreateSyncEventRequest,
            private::sync::models::UpdateSyncEventRequest,
            private::sync::models::SyncCommandResponse,
            private::sync::models::IssueCommandRequest,
            private::sync::models::CreateCredentialRequest,
            private::sync::models::CreateCredentialResponse,
        )
    ),
    tags(
        (name = "health", description = "Health check endpoints"),
        (name = "projects", description = "Project management"),
        (name = "sites", description = "Site management and data"),
        (name = "alarms", description = "Threshold-based alarm violations"),
        (name = "search", description = "Cross-entity search"),
        (name = "ingestion", description = "Data ingestion (readings, status events, grab samples, flagging)"),
        (name = "streams", description = "Data stream registration and pairing"),
        (name = "sensors", description = "Sensor lifecycle: readings, deployment bands, adoption, swapping, calibration windows"),
        (name = "tools", description = "Analytical calculators (DOC, DIC, pCO2, etc.)"),
        (name = "actions", description = "Operator actions: aggregate refresh, recalibration, merging, derived recomputation"),
        (name = "sync", description = "Sync service control plane: discovery, pairing plans, service/credential management"),
        (name = "admin", description = "Keycloak user/role management (require_admin, Keycloak admin role only, no token can pass)"),
    ),
    modifiers(&SecurityAddon),
    // `version` is overwritten at serve time from CARGO_PKG_VERSION (see build_router); the literal
    // here is just a placeholder the derive macro requires.
    info(
        title = "RIVER Data API",
        description = "Time-series sensor data API",
        version = "0.0.0"
    )
)]
struct ApiDoc;

struct SecurityAddon;

impl utoipa::Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "keycloak_jwt",
            utoipa::openapi::security::SecurityScheme::Http(
                utoipa::openapi::security::HttpBuilder::new()
                    .scheme(utoipa::openapi::security::HttpAuthScheme::Bearer)
                    .bearer_format("JWT")
                    .description(Some("Keycloak JWT token (for browser/admin access)"))
                    .build(),
            ),
        );
        components.add_security_scheme(
            "api_token",
            utoipa::openapi::security::SecurityScheme::Http(
                utoipa::openapi::security::HttpBuilder::new()
                    .scheme(utoipa::openapi::security::HttpAuthScheme::Bearer)
                    .description(Some(
                        "API token (created via admin UI, for external scripts/partners)",
                    ))
                    .build(),
            ),
        );
    }
}

/// The private API's OpenAPI document as `/docs` serves it: the hand-written half declared on
/// [`ApiDoc`] plus the entity half the CrudCrate derive generates. Every path is absolute from the
/// server root, so a key in it is a URL the router resolves.
pub fn openapi_spec(state: &AppState) -> utoipa::openapi::OpenApi {
    merged_spec(service::api_router(state).1)
}

/// The document as the committed artefact holds it: pretty-printed, and with the crate version
/// replaced by a fixed marker so a release bump is not a diff in every checked-in spec.
///
/// # Errors
/// When the document cannot be serialised, which is a `ToSchema` derive producing invalid JSON.
pub fn openapi_json(spec: &utoipa::openapi::OpenApi) -> Result<String, serde_json::Error> {
    let mut value = serde_json::to_value(spec)?;
    if let Some(info) = value
        .get_mut("info")
        .and_then(serde_json::Value::as_object_mut)
    {
        info.insert("version".to_string(), serde_json::json!("{crate}"));
    }
    serde_json::to_string_pretty(&value)
}

/// The document as the committed artefact holds it, built from a router that serves nothing.
///
/// One producer for the dump binary and for the guard that checks the committed copy, so the two
/// cannot describe different routers.
///
/// # Errors
/// When the document cannot be serialised, which is a `ToSchema` derive producing invalid JSON.
pub fn committed_document() -> Result<String, serde_json::Error> {
    let state = AppState::new(
        sea_orm::DatabaseConnection::default(),
        crate::config::Config::for_openapi_document(),
        None,
    );
    openapi_json(&openapi_spec(&state))
}

/// [`openapi_spec`] over an entity document already built, for the caller that has one.
fn merged_spec(entity: utoipa::openapi::OpenApi) -> utoipa::openapi::OpenApi {
    use utoipa::OpenApi as _;
    let mut openapi = ApiDoc::openapi();
    openapi.info.version = env!("CARGO_PKG_VERSION").to_string();
    openapi.merge(entity);
    openapi
}

/// The router as a service that carries each connection's peer address. Without this the
/// `ConnectInfo` extension the rate limiter keys on is absent and every caller shares one bucket.
#[must_use]
pub fn connected_service(
    app: Router,
) -> axum::extract::connect_info::IntoMakeServiceWithConnectInfo<Router, std::net::SocketAddr> {
    app.into_make_service_with_connect_info::<std::net::SocketAddr>()
}

pub fn build_router(state: AppState) -> Router {
    let config = &state.config;

    if config.disable_rate_limiting {
        tracing::warn!("Rate limiting DISABLED (including public API)");
    } else {
        tracing::info!(
            public_rate = %format!("burst {} / {}s refill", config.public_rate_limit_burst, config.public_rate_limit_period_secs),
            bulk_concurrent = config.bulk_concurrent_limit,
            "Public API rate limiting configured"
        );
    }

    let (api_inner, entity_api) = service::api_router(&state);

    // Public API routes
    let public_routes = public_api::public_router();

    // Apply dual auth (Keycloak JWT OR API token). Auth is the primary gate; a generous per-client-IP
    // limiter sits in front of it so an unauthenticated flood (e.g. of invalid tokens, each costing an
    // argon2 verification) is bounded per source IP without throttling real loggers, which push large
    // batches infrequently and stay well under the limit.
    let api_authed = {
        let mut r = api_inner.layer(middleware::from_fn_with_state(
            state.clone(),
            crate::common::middleware::service_auth_middleware,
        ));
        if let Some(instance) = state.keycloak_auth_instance.clone() {
            use axum_keycloak_auth::{PassthroughMode, layer::KeycloakAuthLayer};
            r = r.layer(
                // No `required_roles` here: the access gate lives in `service_auth_middleware`,
                // which rejects role-less logins with a distinct 403 (`no_river_role`) instead of
                // silently failing JWT validation and misreporting them as 401 via token auth.
                // This also admits future role holders (e.g. admins without a lower level).
                KeycloakAuthLayer::<crate::common::authz::Role>::builder()
                    .instance(instance)
                    .passthrough_mode(PassthroughMode::Pass)
                    .persist_raw_claims(false)
                    .expected_audiences(vec![String::from("account")])
                    .build(),
            );
        } else {
            tracing::warn!("API routes are not protected by Keycloak (API tokens still work)");
        }
        if !config.disable_rate_limiting {
            // `auth_rate_limit_per_second` is a true requests/second rate; tower_governor replenishes
            // one cell per `period`, so convert: period = 1s / rate (a sub-second interval). Layered
            // last → outermost, so the IP check runs before Keycloak/token validation.
            let rate = config.auth_rate_limit_per_second.max(1);
            let period_nanos = (1_000_000_000u64 / rate).max(1);
            let auth_limiter = GovernorConfigBuilder::default()
                .key_extractor(FallbackIpKeyExtractor::new(&config.trusted_proxy_cidrs))
                .period(Duration::from_nanos(period_nanos))
                .burst_size(config.auth_rate_limit_burst)
                .finish()
                .expect("Failed to create authenticated-tier rate limiter");
            r = r.layer(GovernorLayer {
                config: Arc::new(auth_limiter),
            });
        }
        r
    };

    // Build public routes with optional rate limiting
    let public_routes_final = if config.disable_rate_limiting {
        public_routes
    } else {
        // Modest, deliberately separate from the authenticated /api tier: a token
        // bucket of `burst` cells refilled 1 per `period` (default 10 burst, 1/2s ⇒
        // ~30/min). Public data is cache-backed, so this caps abuse without hurting use.
        let public_limiter = GovernorConfigBuilder::default()
            .key_extractor(FallbackIpKeyExtractor::new(&config.trusted_proxy_cidrs))
            .period(Duration::from_secs(config.public_rate_limit_period_secs))
            .burst_size(config.public_rate_limit_burst)
            .finish()
            .expect("Failed to create public rate limiter");

        public_routes.layer(GovernorLayer {
            config: Arc::new(public_limiter),
        })
    };

    // Sync control routes, separate auth path (body-based creds + session tokens,
    // NOT dual auth via service_auth_middleware). These paths live under /sync/* but
    // don't collide with sync admin views which use /sync/services, /sync/credentials etc.
    let sync_control_routes = service::sync_control_router(&state);

    // Combine all API routes under /api.
    // Body limits: api_router manages its own (10MB on batch readings, 1MB on actions);
    // public is unauthenticated; config gets 1MB limit.
    // Sync control paths (/sync/enroll, /sync/heartbeat, etc.) and the unified router's
    // sync admin views (/sync/services, /sync/credentials, etc.) live on different
    // method+path combinations, so .merge() composes them without conflict. Per-router
    // middleware (dual auth vs sync session auth) is preserved by the merge.
    let api_routes = Router::new()
        .merge(api_authed)
        .merge(sync_control_routes.with_state(state.clone()))
        .nest("/public", public_routes_final.with_state(state.clone()))
        .nest(
            "/config",
            Router::new()
                .route("/keycloak", get(config::get_keycloak_config))
                .route("/notifications", get(config::get_notifications_config))
                .layer(RequestBodyLimitLayer::new(1024 * 1024))
                .with_state(state.clone()),
        );

    // Health check routes (NO rate limiting)
    let health_routes = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz));

    // OpenAPI documentation. Pin the Scalar JS version so a compromised
    // `latest` on jsdelivr cannot inject script into the docs page.
    // Keep this in sync with the version in routes/public_api/mod.rs.
    const PINNED_SCALAR_HTML: &str = r#"<!doctype html>
<html>
<head>
    <title>Scalar</title>
    <meta charset="utf-8"/>
    <meta name="viewport" content="width=device-width, initial-scale=1"/>
</head>
<body>
<script id="api-reference" type="application/json">$spec</script>
<script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference@1.57.2"></script>
</body>
</html>"#;
    // Report the crate's actual version in the served OpenAPI doc rather than a hand-maintained
    // literal (which had gone stale at 0.2.0 while the crate was 0.4.2).
    let openapi = merged_spec(entity_api);
    // `/docs` is unauthenticated and ingress-exposed on the same host as `/api`; only the
    // ingress whitelist-source-range (EPFL ranges, in every overlay) keeps the private spec off
    // the open internet. `/healthz` stays cluster-internal. The spec is the operator-facing
    // contract: `tests/smoke/openapi_paths.rs` checks every path in it resolves. Leaving it auth-free
    // also stops the auth-wrapped fallback from answering unmatched root routes with 401 instead of
    // 404.
    let docs_routes = Router::new()
        .merge(Scalar::with_url("/docs", openapi).custom_html(PINNED_SCALAR_HTML))
        .with_state(state.clone());

    // Build CORS layer from config
    let cors = {
        // A loopback origin is not served by a deployed instance: the compiled-in default is the
        // two local dev servers, and an overlay that names no origin would otherwise let anything
        // a browser loads off localhost make credentialed calls.
        let (served, refused) = crate::config::served_cors_origins(
            config.deployment.clone(),
            &config.cors_allowed_origins,
        );
        if !refused.is_empty() {
            tracing::error!(
                refused = ?refused,
                deployment = ?config.deployment,
                "CORS: loopback origins are not served by a deployed instance; \
                 set CORS_ALLOWED_ORIGINS to this deployment's own dashboard origin"
            );
        }
        let origins = &served;
        if config.cors_allowed_origins.is_empty() || origins.iter().any(|o| o == "*") {
            tracing::warn!("CORS: allowing all origins");
            CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any)
                .expose_headers([axum::http::header::CONTENT_RANGE])
        } else {
            let allowed: Vec<axum::http::HeaderValue> =
                origins.iter().filter_map(|o| o.parse().ok()).collect();
            tracing::info!(origins = ?origins, "CORS: restricted origins");
            CorsLayer::new()
                .allow_origin(allowed)
                .allow_methods([
                    axum::http::Method::GET,
                    axum::http::Method::POST,
                    axum::http::Method::PUT,
                    axum::http::Method::PATCH,
                    axum::http::Method::DELETE,
                    axum::http::Method::HEAD,
                    axum::http::Method::OPTIONS,
                ])
                .allow_headers([
                    axum::http::header::AUTHORIZATION,
                    axum::http::header::CONTENT_TYPE,
                    axum::http::header::ACCEPT,
                    axum::http::HeaderName::from_static("x-request-id"),
                ])
                .allow_credentials(true)
                .expose_headers([
                    axum::http::header::CONTENT_RANGE,
                    axum::http::HeaderName::from_static("x-request-id"),
                ])
        }
    };

    let timeout = Duration::from_secs(config.request_timeout_seconds);
    tracing::info!(
        timeout_seconds = config.request_timeout_seconds,
        "Request timeout configured"
    );

    // Combine all routes. The API is versioned at /api/; health and docs stay at root.
    // All sub-routers have state already bound (Router<()>), so the top-level Router is
    // also Router<()>, no trailing .with_state() needed.
    Router::new()
        .nest("/api", api_routes)
        .merge(health_routes.with_state(state.clone()))
        .merge(docs_routes)
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, timeout))
        .layer(CompressionLayer::new())
        .layer(cors)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(
                    DefaultMakeSpan::new()
                        .level(Level::DEBUG)
                        .include_headers(false),
                )
                .on_request(|req: &axum::http::Request<_>, _span: &tracing::Span| {
                    let request_id = req
                        .headers()
                        .get("x-request-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("-");
                    tracing::debug!(request_id = %request_id, "--> {} {}", req.method(), req.uri().path());
                })
                .on_response(
                    |res: &axum::http::Response<_>,
                     latency: std::time::Duration,
                     _span: &tracing::Span| {
                        let status = res.status();
                        let ms = latency.as_millis();
                        crate::common::request_metrics::record(
                            status.as_u16(),
                            u64::try_from(ms).unwrap_or(u64::MAX),
                        );
                        // A request that went wrong keeps its own line; the rest are counted and
                        // reported per interval by `request_metrics`.
                        if status.is_server_error() {
                            tracing::error!("<-- {} {ms}ms", status);
                        } else if status.is_client_error() {
                            tracing::warn!("<-- {} {ms}ms", status);
                        } else {
                            tracing::debug!("<-- {} {ms}ms", status);
                        }
                    },
                ),
        )
        // `MakeRequestUuid` keeps an inbound id and mints one otherwise, so the id a caller sent is
        // the id the trace and the response carry. `SetRequestIdLayer` must be outermost.
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
}

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;
