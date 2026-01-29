pub mod sensors;
pub mod stations;
pub mod zones;

// Re-export cache from services for use in route handlers
pub use crate::services::cache;

use axum::{http::StatusCode, routing::get, Router};
use sea_orm::{Condition, DatabaseConnection, EntityTrait, QueryFilter, sea_query::Expr};
use std::sync::Arc;
use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};
use uuid::Uuid;

use crate::services::FallbackIpKeyExtractor;
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    trace::TraceLayer,
};
use utoipa::OpenApi;
use utoipa_scalar::{Scalar, Servable};

use crate::common::AppState;
use crate::entity::{stations as stations_entity, zones as zones_entity};
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

/// Resolve a zone by UUID or name (case-insensitive)
pub async fn resolve_zone(
    db: &DatabaseConnection,
    id_or_name: &str,
) -> AppResult<zones_entity::Model> {
    // Try UUID first
    if let Ok(uuid) = id_or_name.parse::<Uuid>() {
        return zones_entity::Entity::find_by_id(uuid)
            .one(db)
            .await?
            .ok_or_else(|| AppError::NotFound("Zone not found".to_string()));
    }

    // Fall back to case-insensitive name lookup using LOWER()
    zones_entity::Entity::find()
        .filter(
            Condition::all().add(
                Expr::cust_with_values("LOWER(name) = LOWER($1)", [id_or_name])
            )
        )
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Zone not found".to_string()))
}

/// Resolve a station by UUID or name (case-insensitive)
pub async fn resolve_station(
    db: &DatabaseConnection,
    id_or_name: &str,
) -> AppResult<stations_entity::Model> {
    // Try UUID first
    if let Ok(uuid) = id_or_name.parse::<Uuid>() {
        return stations_entity::Entity::find_by_id(uuid)
            .one(db)
            .await?
            .ok_or_else(|| AppError::NotFound("Station not found".to_string()));
    }

    // Fall back to case-insensitive name lookup using LOWER()
    stations_entity::Entity::find()
        .filter(
            Condition::all().add(
                Expr::cust_with_values("LOWER(name) = LOWER($1)", [id_or_name])
            )
        )
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Station not found".to_string()))
}

// ============================================================================
// OpenAPI Documentation
// ============================================================================

#[derive(OpenApi)]
#[openapi(
    paths(
        healthz,
        zones::list_zones,
        zones::get_zone,
        zones::list_zone_stations,
        stations::list_stations,
        stations::get_station,
        stations::list_station_sensors,
        stations::get_station_readings,
        stations::get_station_aggregates,
        sensors::list_sensors,
    ),
    components(
        schemas(
            zones::ZoneResponse,
            stations::StationResponse,
            stations::StationDetailResponse,
            stations::StationRef,
            stations::ZoneRef,
            sensors::SensorResponse,
            stations::ReadingsResponse,
            stations::SensorData,
            stations::AggregatesResponse,
            stations::SensorAggregateData,
        )
    ),
    tags(
        (name = "health", description = "Health check endpoints"),
        (name = "zones", description = "Zone management"),
        (name = "stations", description = "Station management and data"),
        (name = "sensors", description = "Sensor management"),
    ),
    info(
        title = "River DB API",
        description = "Time-series sensor data API for Vaisala viewLinc",
        version = "0.1.0"
    )
)]
struct ApiDoc;

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

    // Metadata routes (zones, stations, sensors listings)
    let metadata_routes_base = Router::new()
        .route("/zones", get(zones::list_zones))
        .route("/zones/{zone_id}", get(zones::get_zone))
        .route("/zones/{zone_id}/stations", get(zones::list_zone_stations))
        .route("/stations", get(stations::list_stations))
        .route("/stations/{station_id}", get(stations::get_station))
        .route("/stations/{station_id}/sensors", get(stations::list_station_sensors))
        .route("/sensors", get(sensors::list_sensors));

    // Data routes (readings, aggregates)
    let data_routes_base = Router::new()
        .route(
            "/stations/{station_id}/readings",
            get(stations::get_station_readings),
        )
        .route(
            "/stations/{station_id}/aggregates/{resolution}",
            get(stations::get_station_aggregates),
        );

    // Combine API routes, conditionally applying rate limiting
    let api_routes = if config.disable_rate_limiting {
        Router::new()
            .merge(metadata_routes_base)
            .merge(data_routes_base)
    } else {
        let metadata_limiter = GovernorConfigBuilder::default()
            .key_extractor(FallbackIpKeyExtractor)
            .per_second(config.rate_limit_metadata_per_second)
            .burst_size(config.rate_limit_metadata_burst)
            .finish()
            .expect("Failed to create metadata rate limiter");

        let data_limiter = GovernorConfigBuilder::default()
            .key_extractor(FallbackIpKeyExtractor)
            .per_second(config.rate_limit_data_per_second)
            .burst_size(config.rate_limit_data_burst)
            .finish()
            .expect("Failed to create data rate limiter");

        Router::new()
            .merge(metadata_routes_base.layer(GovernorLayer {
                config: Arc::new(metadata_limiter),
            }))
            .merge(data_routes_base.layer(GovernorLayer {
                config: Arc::new(data_limiter),
            }))
    }
    .layer(RequestBodyLimitLayer::new(1024 * 1024)); // 1MB body limit

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
                .allow_headers(Any),
        )
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
