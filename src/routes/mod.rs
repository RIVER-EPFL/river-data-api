pub mod admin;
pub mod alarms;
pub mod config;
pub mod projects;
pub mod public_api;
pub mod service;
pub mod sites;

// Re-export cache from services for use in route handlers
pub use crate::services::cache;

use axum::{Router, http::StatusCode, middleware, routing::get};
use sea_orm::{Condition, DatabaseConnection, EntityTrait, QueryFilter, sea_query::Expr};
use std::sync::Arc;
use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};
use uuid::Uuid;

use crate::services::FallbackIpKeyExtractor;
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    trace::{DefaultMakeSpan, TraceLayer},
};
use tracing::Level;
use utoipa::OpenApi;
use utoipa_scalar::{Scalar, Servable};

use crate::common::AppState;
use crate::entity::{projects as projects_entity, sites as sites_entity};
use crate::error::{AppError, AppResult};

// ============================================================================
// Root Endpoints
// ============================================================================

/// Health check endpoint
///
/// Returns 200 OK if the service is running.
/// This endpoint is not rate-limited and suitable for Kubernetes probes.
#[utoipa::path(
    get,
    path = "/healthz",
    responses(
        (status = 200, description = "Service is healthy"),
    ),
    tag = "health"
)]
async fn healthz() -> StatusCode {
    StatusCode::OK
}

// ============================================================================
// Resolution Helpers
// ============================================================================

/// Resolve a project by UUID or name (case-insensitive)
pub async fn resolve_project(
    db: &DatabaseConnection,
    id_or_name: &str,
) -> AppResult<projects_entity::Model> {
    // Try UUID first
    if let Ok(uuid) = id_or_name.parse::<Uuid>() {
        return projects_entity::Entity::find_by_id(uuid)
            .one(db)
            .await?
            .ok_or_else(|| AppError::NotFound("Project not found".to_string()));
    }

    // Fall back to case-insensitive name lookup using LOWER()
    projects_entity::Entity::find()
        .filter(Condition::all().add(Expr::cust_with_values(
            "LOWER(name) = LOWER($1)",
            [id_or_name],
        )))
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Project not found".to_string()))
}

/// Resolve a site by UUID or name (case-insensitive)
pub async fn resolve_site(
    db: &DatabaseConnection,
    id_or_name: &str,
) -> AppResult<sites_entity::Model> {
    // Try UUID first
    if let Ok(uuid) = id_or_name.parse::<Uuid>() {
        return sites_entity::Entity::find_by_id(uuid)
            .one(db)
            .await?
            .ok_or_else(|| AppError::NotFound("Site not found".to_string()));
    }

    // Fall back to case-insensitive name lookup using LOWER()
    sites_entity::Entity::find()
        .filter(Condition::all().add(Expr::cust_with_values(
            "LOWER(name) = LOWER($1)",
            [id_or_name],
        )))
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Site not found".to_string()))
}

// ============================================================================
// OpenAPI Documentation
// ============================================================================

#[derive(OpenApi)]
#[openapi(
    paths(
        healthz,
        projects::list_projects,
        projects::get_project,
        projects::list_project_sites,
        sites::list_sites,
        sites::get_site,
        sites::list_site_parameters,
        sites::get_site_readings,
        sites::get_site_aggregates,
        alarms::get_site_alarms,
    ),
    components(
        schemas(
            projects::ProjectResponse,
            sites::SiteResponse,
            sites::SiteDetailResponse,
            sites::SiteRef,
            sites::ProjectRef,
            sites::ParameterResponse,
            sites::ReadingsResponse,
            sites::ParameterData,
            sites::AggregatesResponse,
            sites::ParameterAggregateData,
            alarms::AlarmViolationsResponse,
            alarms::ParameterViolationData,
        )
    ),
    tags(
        (name = "health", description = "Health check endpoints"),
        (name = "projects", description = "Project management"),
        (name = "sites", description = "Site management and data"),
        (name = "alarms", description = "Threshold-based alarm violations"),
    ),
    modifiers(&SecurityAddon),
    info(
        title = "River Data API",
        description = "Time-series sensor data API",
        version = "0.2.0"
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

// ============================================================================
// Router Builder
// ============================================================================

pub fn build_router(state: AppState) -> Router {
    let config = &state.config;

    if config.disable_rate_limiting {
        tracing::warn!("Rate limiting DISABLED");
    } else {
        tracing::info!(
            metadata_rate = %format!("{}/s burst {}", config.rate_limit_metadata_per_second, config.rate_limit_metadata_burst),
            data_rate = %format!("{}/s burst {}", config.rate_limit_data_per_second, config.rate_limit_data_burst),
            bulk_concurrent = config.bulk_concurrent_limit,
            "Rate limiting configured"
        );
    }

    // Service tier router (replaces old /api/private/)
    let service_routes_inner = service::service_router(&state);

    // Public API routes
    let public_routes = public_api::public_router();

    // Admin routes (Keycloak-protected)
    let admin_routes = admin::admin_router(&state);

    // Apply optional rate limiting to service tier
    let service_routes_rated = if config.disable_rate_limiting {
        service_routes_inner
    } else {
        let service_limiter = GovernorConfigBuilder::default()
            .key_extractor(FallbackIpKeyExtractor)
            .per_second(config.rate_limit_data_per_second)
            .burst_size(config.rate_limit_data_burst)
            .finish()
            .expect("Failed to create service rate limiter");

        service_routes_inner.layer(GovernorLayer {
            config: Arc::new(service_limiter),
        })
    };

    // Apply dual auth (Keycloak JWT OR API token) to service routes
    let service_routes = {
        let mut r = service_routes_rated.layer(middleware::from_fn_with_state(
            state.clone(),
            crate::common::middleware::service_auth_middleware,
        ));
        if let Some(instance) = state.keycloak_auth_instance.clone() {
            use axum_keycloak_auth::{PassthroughMode, layer::KeycloakAuthLayer};
            r = r.layer(
                KeycloakAuthLayer::<crate::common::auth::Role>::builder()
                    .instance(instance)
                    .passthrough_mode(PassthroughMode::Pass)
                    .persist_raw_claims(false)
                    .expected_audiences(vec![String::from("account")])
                    .required_roles(vec![crate::common::auth::Role::User])
                    .build(),
            );
        } else {
            tracing::warn!("Service routes are not protected by Keycloak (API tokens still work)");
        }
        r
    };

    // Build public routes with optional rate limiting
    let public_routes_final = if config.disable_rate_limiting {
        public_routes
    } else {
        let public_limiter = GovernorConfigBuilder::default()
            .key_extractor(FallbackIpKeyExtractor)
            .per_second(config.rate_limit_data_per_second)
            .burst_size(config.rate_limit_data_burst)
            .finish()
            .expect("Failed to create public rate limiter");

        public_routes.layer(GovernorLayer {
            config: Arc::new(public_limiter),
        })
    };

    // Combine all API routes
    // Body limits: service tier manages its own (10MB on batch readings),
    // admin and config get 1MB limit.
    let api_routes = Router::new()
        .nest("/service", service_routes)
        .nest("/public", public_routes_final)
        .nest(
            "/admin",
            admin_routes.layer(RequestBodyLimitLayer::new(1024 * 1024)),
        )
        .nest(
            "/config",
            Router::new()
                .route("/keycloak", get(config::get_keycloak_config))
                .layer(RequestBodyLimitLayer::new(1024 * 1024)),
        );

    // Health check routes (NO rate limiting)
    let health_routes = Router::new().route("/healthz", get(healthz));

    // OpenAPI documentation
    let docs_routes = Router::new().merge(Scalar::with_url("/docs", ApiDoc::openapi()));

    // Combine all routes
    Router::new()
        .nest("/api", api_routes)
        .merge(health_routes)
        .merge(docs_routes)
        .layer(CompressionLayer::new())
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any)
                .expose_headers([axum::http::header::CONTENT_RANGE]),
        )
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_request(|req: &axum::http::Request<_>, _span: &tracing::Span| {
                    tracing::info!("--> {} {}", req.method(), req.uri().path());
                })
                .on_response(
                    |res: &axum::http::Response<_>,
                     latency: std::time::Duration,
                     _span: &tracing::Span| {
                        let status = res.status();
                        let ms = latency.as_millis();
                        if status.is_server_error() {
                            tracing::error!("<-- {} {ms}ms", status);
                        } else if status.is_client_error() {
                            tracing::warn!("<-- {} {ms}ms", status);
                        } else {
                            tracing::info!("<-- {} {ms}ms", status);
                        }
                    },
                ),
        )
        .with_state(state)
}
