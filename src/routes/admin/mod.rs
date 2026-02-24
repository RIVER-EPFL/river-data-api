pub mod calibrations;
pub mod derived;
pub mod imports;
pub mod sync;

use axum::{routing::post, Router};
use crate::common::AppState;
use crate::common::auth::Role;
use crate::entity::{
    alarm_thresholds::AlarmThreshold,
    api_tokens::ApiToken,
    data_imports::DataImport,
    derived_parameter_definitions::DerivedParameterDefinition,
    parameter_types::ParameterType,
    parameters::Parameter,
    projects::Project,
    sensor_calibrations::SensorCalibration,
    sensor_deployments::SensorDeployment,
    sensors::Sensor,
    sites::Site,
};

pub fn admin_router(state: &AppState) -> Router<AppState> {
    let db = &state.db;

    // Convert crudcrate OpenApiRouters to axum::Router for nesting
    let crud = |r: utoipa_axum::router::OpenApiRouter| -> Router<()> { r.into() };

    let mut router = Router::new()
        .nest("/sync", sync::router())
        // crudcrate-generated CRUD routers (state self-contained via DatabaseConnection)
        .nest_service("/projects", crud(Project::router(db)))
        .nest_service("/sites", crud(Site::router(db)))
        .nest_service("/parameter_types", crud(ParameterType::router(db)))
        .nest_service("/parameters", crud(Parameter::router(db)))
        .nest_service("/sensors", crud(Sensor::router(db)))
        .nest_service("/sensor_calibrations", crud(SensorCalibration::router(db)))
        .nest_service("/sensor_deployments", crud(SensorDeployment::router(db)))
        .nest_service("/derived_parameters", crud(DerivedParameterDefinition::router(db)))
        .nest_service("/alarm_thresholds", crud(AlarmThreshold::router(db)))
        .nest_service("/tokens", crud(ApiToken::router(db)))
        .nest_service("/data_imports", crud(DataImport::router(db)))
        // Custom action routes under /actions/ to avoid conflict with nest_service catch-all
        .route("/actions/sensor_calibrations/{id}/recalculate", post(calibrations::recalculate_calibration))
        .route("/actions/derived_parameters/{id}/recompute", post(derived::recompute_derived))
        .route("/actions/data_imports/upload", post(imports::upload_csv));

    // Apply Keycloak auth layer if configured
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
