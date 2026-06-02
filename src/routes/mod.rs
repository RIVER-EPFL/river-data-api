pub mod config;
pub mod private;
pub mod public;
pub mod public_api;
pub mod service;

pub use crate::common::cache;

use axum::{Router, http::StatusCode, middleware, response::Response, routing::get};
use sea_orm::{Condition, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter, Statement, sea_query::Expr};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceBuilder;
use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};
use uuid::Uuid;

use crate::common::rate_limit::FallbackIpKeyExtractor;
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
use crate::routes::private::{projects as projects_entity, sites as sites_entity};
use crate::error::{AppError, AppResult};

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
        if span > chrono::Duration::days(max_days) {
            return Err(AppError::BadRequest(format!(
                "Time range exceeds maximum of {max_days} days"
            )));
        }
    } else {
        // No end specified — check span against now
        let span = chrono::Utc::now() - effective_start;
        if span > chrono::Duration::days(max_days) {
            return Err(AppError::BadRequest(format!(
                "Time range exceeds maximum of {max_days} days (provide a narrower start or add an end time)"
            )));
        }
    }

    Ok((effective_start, end))
}

#[derive(OpenApi)]
#[openapi(
    paths(
        healthz,
        private::projects::views::list_project_sites,
        private::sites::handlers::list_site_parameters,
        private::sites::handlers::get_site_detail,
        private::sites::readings::get_site_readings,
        private::sites::aggregates::get_site_aggregates,
        private::sites::status_events::get_site_status_events,
        private::alarms::views::get_site_alarms,
        private::sites::annotations::get_site_annotations,
        private::search::search,
        private::readings::flags::flag_readings,
        private::readings::flags::unflag_readings,
        private::readings::flags::flag_range,
        private::readings::flags::unflag_range,
        private::readings::ingest::ingest_readings,
        private::readings::ingest::ingest_status_events,
        private::readings::batch::insert_batch_readings,
        private::readings::grab_samples::insert_grab_samples,
        private::status_events::batch::insert_batch_status_events,
        private::data_streams::views::stream_stats,
        private::data_streams::views::register_stream,
        private::data_streams::views::pair_stream,
        private::data_streams::views::unpair_stream,
        private::tools::list_tools,
        private::tools::calculate_tool,
        private::admin::actions::refresh_aggregates,
        private::admin::actions::compute_derived,
        private::admin::actions::rollback_deployment,
        private::admin::actions::preview_derived,
        private::admin::calibrations::recalculate_calibration,
        private::admin::derived::recompute_derived,
        private::admin::merge::merge_site_parameters_handler,
        private::admin::merge::merge_parameters_handler,
        private::admin::public_config::invalidate_public_config,
        private::sync::views::get_discovery,
        private::sync::views::apply_discovery,
        private::sync::views::grouped_discovery,
        private::sync::views::bulk_pair,
        private::sync::views::create_pairing_plan,
        private::sync::views::list_pairing_plans,
        private::sync::views::get_pairing_plan,
        private::sync::views::update_pairing_plan,
        private::sync::views::apply_pairing_plan,
        private::sync::views::revert_pairing_plan,
        private::sync::views::unpaired_summary,
        private::sync::views::plan_site_metadata,
        private::admin::users::list_users,
        private::admin::users::get_user,
        private::admin::users::create_user,
        private::admin::users::update_user,
        private::admin::users::delete_user,
        private::admin::users::assign_roles,
        private::admin::users::list_roles,
        river_data_core::server::handlers::enroll::enroll,
        river_data_core::server::handlers::heartbeat::heartbeat,
        river_data_core::server::handlers::commands::update_command,
        river_data_core::server::handlers::events::create_sync_event,
        river_data_core::server::handlers::events::update_sync_event,
        river_data_core::server::handlers::admin::list_services,
        river_data_core::server::handlers::admin::get_service,
        river_data_core::server::handlers::admin::issue_command,
        river_data_core::server::handlers::admin::list_commands,
        river_data_core::server::handlers::admin::list_credentials,
        river_data_core::server::handlers::admin::create_credential,
        river_data_core::server::handlers::admin::revoke_credential,
        river_data_core::server::handlers::admin::list_sync_events,
        river_data_core::server::handlers::admin::revoke_service,
    ),
    components(
        schemas(
            private::projects::types::ProjectResponse,
            private::sites::types::SiteResponse,
            private::sites::types::SiteDetailResponse,
            private::sites::types::SiteRef,
            private::sites::types::ProjectRef,
            private::sites::types::ParameterResponse,
            private::sites::readings::ReadingsResponse,
            private::sites::readings::ParameterData,
            private::sites::aggregates::AggregatesResponse,
            private::sites::aggregates::ParameterAggregateData,
            private::sites::status_events::StatusEventsResponse,
            private::sites::annotations::AnnotationResponse,
            private::alarms::types::AlarmViolationsResponse,
            private::alarms::types::ParameterViolationData,
            private::search::SearchResponse,
            private::search::SearchResults,
            private::search::SiteResult,
            private::search::SensorResult,
            private::search::ParameterResult,
            private::search::ProjectResult,
            private::readings::flags::ReadingKey,
            private::readings::flags::FlagReadingsRequest,
            private::readings::flags::UnflagReadingsRequest,
            private::readings::flags::FlagReadingsResponse,
            private::readings::flags::FlagRangeRequest,
            private::readings::flags::UnflagRangeRequest,
            private::readings::ingest::IngestReadingsRequest,
            private::readings::ingest::IngestReading,
            private::readings::ingest::IngestResponse,
            private::readings::ingest::IngestStatusEventsRequest,
            private::readings::ingest::IngestStatusEvent,
            private::readings::ingest::IngestStatusEventsResponse,
            private::readings::batch::BatchReadingsRequest,
            private::readings::batch::ReadingInput,
            private::readings::batch::BatchReadingsResponse,
            private::readings::grab_samples::GrabSampleRequest,
            private::readings::grab_samples::GrabSampleReading,
            private::readings::grab_samples::GrabSampleResponse,
            private::status_events::batch::BatchStatusEventsRequest,
            private::status_events::batch::StatusEventInput,
            private::status_events::batch::BatchStatusEventsResponse,
            private::data_streams::views::StreamStatsResponse,
            private::data_streams::views::RegisterStreamRequest,
            private::data_streams::views::StreamResponse,
            private::data_streams::views::PairStreamRequest,
            private::data_streams::views::PairStreamResponse,
            private::data_streams::views::UnpairStreamResponse,
            private::tools::ToolResult,
            private::tools::ToolParamInfo,
            private::tools::ToolInfo,
            private::admin::actions::RefreshAggregatesRequest,
            private::admin::actions::ComputeDerivedRequest,
            private::admin::actions::SiteTimestamps,
            private::admin::actions::RollbackDeploymentRequest,
            private::admin::actions::RollbackDeploymentResponse,
            private::admin::actions::PreviewDerivedRequest,
            private::admin::actions::PreviewDerivedResponse,
            private::admin::actions::PreviewSite,
            private::admin::actions::SourceParameterSeries,
            private::admin::actions::DerivedSeries,
            private::admin::merge_services::MergeSiteParametersRequest,
            private::admin::merge_services::MergeSiteParametersResponse,
            private::admin::merge_services::MergeParametersRequest,
            private::admin::merge_services::MergeParametersResponse,
            private::admin::users::CreateUserRequest,
            private::admin::users::AssignRolesRequest,
            private::admin::users::KeycloakRole,
            river_data_core::models::EnrollRequest,
            river_data_core::models::EnrollResponse,
            river_data_core::models::HeartbeatRequest,
            river_data_core::models::HeartbeatResponse,
            river_data_core::models::PendingCommand,
            river_data_core::models::CommandUpdateRequest,
            river_data_core::server::handlers::events::CreateSyncEventRequest,
            river_data_core::server::handlers::events::UpdateSyncEventRequest,
            river_data_core::server::handlers::admin::SyncServiceResponse,
            river_data_core::server::handlers::admin::SyncCommandResponse,
            river_data_core::server::handlers::admin::IssueCommandRequest,
            river_data_core::server::handlers::admin::CreateCredentialRequest,
            river_data_core::server::handlers::admin::CreateCredentialResponse,
            river_data_core::server::handlers::admin::CredentialResponse,
            river_data_core::server::handlers::admin::SyncEventResponse,
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
        (name = "tools", description = "Analytical calculators (DOC, DIC, pCO2, etc.)"),
        (name = "actions", description = "Operator actions: aggregate refresh, recalibration, merging, derived recomputation"),
        (name = "sync", description = "Sync service control plane: discovery, pairing plans, service/credential management"),
        (name = "admin", description = "Keycloak user/role management (require_admin — Keycloak admin role only, no token can pass)"),
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

    // Unified /api/ router. Hosts everything previously split between /api/admin/
    // and /api/service/. Per-route authorization lives in api_router itself via the
    // require_* middleware; here we layer dual-auth and rate limiting on top.
    let api_inner = service::api_router(&state);

    // Public API routes
    let public_routes = public_api::public_router();

    // Apply optional rate limiting
    let api_rated = if config.disable_rate_limiting {
        api_inner
    } else {
        let api_limiter = GovernorConfigBuilder::default()
            .key_extractor(FallbackIpKeyExtractor)
            .per_second(config.rate_limit_data_per_second)
            .burst_size(config.rate_limit_data_burst)
            .finish()
            .expect("Failed to create api rate limiter");

        api_inner.layer(GovernorLayer {
            config: Arc::new(api_limiter),
        })
    };

    // Apply dual auth (Keycloak JWT OR API token)
    let api_authed = {
        let mut r = api_rated.layer(middleware::from_fn_with_state(
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
            tracing::warn!("API routes are not protected by Keycloak (API tokens still work)");
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
            .key_extractor(FallbackIpKeyExtractor)
            .period(Duration::from_secs(config.public_rate_limit_period_secs))
            .burst_size(config.public_rate_limit_burst)
            .finish()
            .expect("Failed to create public rate limiter");

        public_routes.layer(GovernorLayer {
            config: Arc::new(public_limiter),
        })
    };

    // Sync control routes — separate auth path (body-based creds + session tokens,
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
    let docs_routes = Router::new()
        .merge(Scalar::with_url("/docs", ApiDoc::openapi()).custom_html(PINNED_SCALAR_HTML));

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

    // Combine all routes. The API is versioned at /api/; health and docs stay at root.
    // All sub-routers have state already bound (Router<()>), so the top-level Router is
    // also Router<()> — no trailing .with_state() needed.
    Router::new()
        .nest("/api", api_routes)
        .merge(health_routes.with_state(state.clone()))
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
