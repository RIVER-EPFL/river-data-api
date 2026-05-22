use axum::{Router, routing::{get, post}};

use crate::common::AppState;
use crate::common::auth::Role;
use crate::routes::private::{
    alarm_thresholds::AlarmThreshold,
    api_tokens::ApiToken,
    constants::Constant,
    data_streams::DataStream,
    derived_parameters::definition_model::DerivedParameterDefinition,
    derived_parameters::source_model::DerivedParameterSource,
    notes::Note,
    parameters::Parameter,
    projects::Project,
    public_exposed_parameters::PublicExposedParameter,
    reprocessing_jobs::ReprocessingJob,
    samples::Sample,
    sensor_calibrations::SensorCalibration,
    sensor_deployments::SensorDeployment,
    sensors::Sensor,
    site_parameters::SiteParameter,
    sites::Site,
    standard_curves::StandardCurve,
    sync::commands_model::SyncCommand,
    sync::credentials_model::SyncServiceCredential,
    sync::events_model::SyncEvent,
    sync::services_model::SyncService,
};

use crate::routes::private::{
    admin::{calibrations, derived, merge, public_config, users},
    alarms::views as alarm_views,
    search,
    sync::views as sync_views,
};

pub fn admin_router(state: &AppState) -> Router<AppState> {
    let db = &state.db;

    let crud = |r: utoipa_axum::router::OpenApiRouter| -> Router<()> { r.into() };

    let mut router = Router::new()
        .nest("/sync", sync_views::router())
        .nest_service("/projects", crud(Project::router(db)))
        .nest_service("/sites", crud(Site::router(db)))
        .nest_service("/parameters", crud(Parameter::router(db)))
        .nest_service("/site_parameters", crud(SiteParameter::router(db)))
        .nest_service("/sensors", crud(Sensor::router(db)))
        .nest_service("/sensor_calibrations", crud(SensorCalibration::router(db)))
        .nest_service("/sensor_deployments", crud(SensorDeployment::router(db)))
        .nest_service("/derived_parameters", crud(DerivedParameterDefinition::router(db)))
        .nest_service("/derived_parameter_sources", crud(DerivedParameterSource::router(db)))
        .nest_service("/alarm_thresholds", crud(AlarmThreshold::router(db)))
        .nest_service("/tokens", crud(ApiToken::router(db)))
        .nest_service("/public_exposed_parameters", crud(PublicExposedParameter::router(db)))
        .nest_service("/standard_curves", crud(StandardCurve::router(db)))
        .nest_service("/notes", crud(Note::router(db)))
        .nest_service("/constants", crud(Constant::router(db)))
        .nest_service("/samples", crud(Sample::router(db)))
        .nest_service("/reprocessing_jobs", crud(ReprocessingJob::router(db)))
        .nest_service("/data_streams", crud(DataStream::router(db)))
        .nest_service("/sync_services", crud(SyncService::router(db)))
        .nest_service("/sync_commands", crud(SyncCommand::router(db)))
        .nest_service("/sync_events", crud(SyncEvent::router(db)))
        .nest_service("/sync_service_credentials", crud(SyncServiceCredential::router(db)))
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
        .route("/actions/merge_site_parameters", post(merge::merge_site_parameters_handler))
        .route("/actions/merge_parameters", post(merge::merge_parameters_handler))
        .route("/alarms/active", get(alarm_views::get_active_alarms))
        .route("/alarms/summary", get(alarm_views::get_alarm_summary))
        .route("/search", get(search::search))
        .route(
            "/actions/preview_derived",
            post(crate::routes::private::admin::actions::preview_derived),
        )
        .route(
            "/actions/rollback_deployment",
            post(crate::routes::private::admin::actions::rollback_deployment),
        );

    if state.keycloak_admin.is_some() {
        router = router
            .nest("/users", users::router())
            .route("/roles", get(users::list_roles));
        tracing::info!("User management routes enabled");
    }

    if let Some(instance) = state.keycloak_auth_instance.clone() {
        use axum_keycloak_auth::{PassthroughMode, layer::KeycloakAuthLayer};
        router = router.layer(
            KeycloakAuthLayer::<Role>::builder()
                .instance(instance)
                .passthrough_mode(PassthroughMode::Block)
                .persist_raw_claims(false)
                .expected_audiences(vec![String::from("account")])
                .required_roles(vec![Role::Administrator])
                .build(),
        );
    } else {
        tracing::warn!("Admin routes are not protected by authentication");
    }

    router
}
