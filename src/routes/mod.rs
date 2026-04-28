pub mod admin;
pub mod alarms;
pub mod config;
pub mod projects;
pub mod public_api;
pub mod service;
pub mod sites;

// Re-export cache from services for use in route handlers
pub use crate::services::cache;

use axum::{Router, http::StatusCode, middleware, response::Response, routing::get};
use sea_orm::{Condition, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter, Statement, sea_query::Expr};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceBuilder;
use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};
use uuid::Uuid;

use crate::services::FallbackIpKeyExtractor;
use tower_http::{
    compression::CompressionLayer,
    cors::CorsLayer,
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

/// Liveness probe — returns 200 if the process is running.
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

/// Readiness probe — returns 200 only if the database is reachable.
#[utoipa::path(
    get,
    path = "/readyz",
    responses(
        (status = 200, description = "Service is ready"),
        (status = 503, description = "Database unreachable"),
    ),
    tag = "health"
)]
async fn readyz(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> StatusCode {
    let result = state
        .db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT 1".to_string(),
        ))
        .await;
    match result {
        Ok(Some(_)) => StatusCode::OK,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    }
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

// ============================================================================
// Time Range Validation
// ============================================================================

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

/// Enforce a maximum time range span and require `start`.
/// If `start` is None, defaults to `now - default_lookback_days`.
/// Returns the effective (start, end) after applying defaults and validation.
pub fn enforce_time_range(
    start: Option<chrono::DateTime<chrono::Utc>>,
    end: Option<chrono::DateTime<chrono::Utc>>,
    max_days: i64,
    default_lookback_days: i64,
) -> AppResult<(chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>)> {
    let effective_start = start.unwrap_or_else(|| {
        chrono::Utc::now() - chrono::Duration::days(default_lookback_days)
    });

    if let Some(e) = end {
        if e < effective_start {
            return Err(AppError::BadRequest(
                "end time must not be before start time".to_string(),
            ));
        }
        let span = e - effective_start;
        if span.num_days() > max_days {
            return Err(AppError::BadRequest(format!(
                "Time range exceeds maximum of {max_days} days"
            )));
        }
    } else {
        // No end specified — check span against now
        let span = chrono::Utc::now() - effective_start;
        if span.num_days() > max_days {
            return Err(AppError::BadRequest(format!(
                "Time range exceeds maximum of {max_days} days (provide a narrower start or add an end time)"
            )));
        }
    }

    Ok((effective_start, end))
}

// ============================================================================
// OpenAPI Documentation
// ============================================================================

#[derive(OpenApi)]
#[openapi(
    paths(
        healthz,
        projects::list_project_sites,
        sites::list_site_parameters,
        sites::get_site_detail,
        sites::get_site_readings,
        sites::get_site_aggregates,
        sites::get_site_status_events,
        alarms::get_site_alarms,
        sites::get_site_annotations,
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
            sites::StatusEventsResponse,
            sites::AnnotationResponse,
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

    // Sync control routes — separate auth path (body-based creds + session tokens,
    // NOT dual auth via service_auth_middleware)
    let sync_control_routes = service::sync_control_router(&state);

    // Combine all API routes
    // Body limits: service tier manages its own (10MB on batch readings),
    // admin and config get 1MB limit.
    // Note: nest() routes take priority over nest_service() wildcards,
    // so sync control paths are matched before the service catch-all.
    let api_routes = Router::new()
        .nest_service("/service", service_routes)
        .nest("/service", sync_control_routes)
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
    let health_routes = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz));

    // OpenAPI documentation
    let docs_routes = Router::new().merge(Scalar::with_url("/docs", ApiDoc::openapi()));

    // Build CORS layer from config
    let cors = {
        let origins = &config.cors_allowed_origins;
        if origins.is_empty() || origins.iter().any(|o| o == "*") {
            tracing::warn!("CORS: allowing all origins");
            CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any)
                .expose_headers([axum::http::header::CONTENT_RANGE])
        } else {
            let allowed: Vec<axum::http::HeaderValue> = origins
                .iter()
                .filter_map(|o| o.parse().ok())
                .collect();
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
    tracing::info!(timeout_seconds = config.request_timeout_seconds, "Request timeout configured");

    // Combine all routes
    Router::new()
        .nest("/api", api_routes)
        .merge(health_routes)
        .merge(docs_routes)
        .layer(
            ServiceBuilder::new()
                .layer(axum::error_handling::HandleErrorLayer::new(|_: tower::BoxError| async {
                    StatusCode::REQUEST_TIMEOUT
                }))
                .timeout(timeout),
        )
        .layer(CompressionLayer::new())
        .layer(cors)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(
                    DefaultMakeSpan::new()
                        .level(Level::INFO)
                        .include_headers(false),
                )
                .on_request(|req: &axum::http::Request<_>, _span: &tracing::Span| {
                    let request_id = req
                        .headers()
                        .get("x-request-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("-");
                    tracing::info!(request_id = %request_id, "--> {} {}", req.method(), req.uri().path());
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
        .layer(axum::middleware::from_fn(request_id_middleware))
        .with_state(state)
}

/// Middleware that generates a unique request ID for each request.
async fn request_id_middleware(
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    request.headers_mut().insert(
        axum::http::HeaderName::from_static("x-request-id"),
        axum::http::HeaderValue::from_str(&request_id).unwrap_or_else(|_| {
            axum::http::HeaderValue::from_static("unknown")
        }),
    );

    let mut response = next.run(request).await;
    if let Ok(val) = axum::http::HeaderValue::from_str(&request_id) {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static("x-request-id"),
            val,
        );
    }
    response
}
