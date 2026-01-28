pub mod aggregates;
pub mod cache;
pub mod health;
pub mod hierarchy;
mod rate_limit;
pub mod readings;

use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Condition, sea_query::Expr};
use std::sync::Arc;
use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};
use uuid::Uuid;

use rate_limit::FallbackIpKeyExtractor;
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    trace::TraceLayer,
};
use utoipa::OpenApi;
use utoipa_scalar::{Scalar, Servable};

use crate::common::AppState;
use crate::entity::{sensors, stations, zones};
use crate::error::{AppError, AppResult};

/// Resolve a zone by UUID or name (case-insensitive)
pub async fn resolve_zone(
    db: &DatabaseConnection,
    id_or_name: &str,
) -> AppResult<zones::Model> {
    // Try UUID first
    if let Ok(uuid) = id_or_name.parse::<Uuid>() {
        return zones::Entity::find_by_id(uuid)
            .one(db)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Zone '{id_or_name}' not found")));
    }

    // Fall back to case-insensitive name lookup using LOWER()
    zones::Entity::find()
        .filter(
            Condition::all().add(
                Expr::cust_with_values("LOWER(name) = LOWER($1)", [id_or_name])
            )
        )
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Zone '{id_or_name}' not found")))
}

/// Resolve a station by UUID or name (case-insensitive)
pub async fn resolve_station(
    db: &DatabaseConnection,
    id_or_name: &str,
) -> AppResult<stations::Model> {
    // Try UUID first
    if let Ok(uuid) = id_or_name.parse::<Uuid>() {
        return stations::Entity::find_by_id(uuid)
            .one(db)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Station '{id_or_name}' not found")));
    }

    // Fall back to case-insensitive name lookup using LOWER()
    stations::Entity::find()
        .filter(
            Condition::all().add(
                Expr::cust_with_values("LOWER(name) = LOWER($1)", [id_or_name])
            )
        )
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Station '{id_or_name}' not found")))
}

#[derive(OpenApi)]
#[openapi(
    paths(
        health::healthz,
        hierarchy::list_zones,
        hierarchy::list_stations,
        hierarchy::list_sensors,
        readings::get_station_readings,
        aggregates::get_station_aggregates,
        list_zone_stations,
        list_station_sensors,
    ),
    components(
        schemas(
            hierarchy::ZoneResponse,
            hierarchy::StationResponse,
            hierarchy::SensorResponse,
            readings::ReadingsResponse,
            readings::SensorData,
            aggregates::AggregatesResponse,
            aggregates::SensorAggregateData,
        )
    ),
    tags(
        (name = "health", description = "Health check endpoints"),
        (name = "hierarchy", description = "Zones, stations, and sensors"),
        (name = "readings", description = "Raw sensor readings"),
        (name = "aggregates", description = "Pre-computed aggregates"),
    ),
    info(
        title = "River DB API",
        description = "Time-series sensor data API for Vaisala viewLinc",
        version = "0.1.0"
    )
)]
struct ApiDoc;

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

    // Base routes without rate limiting
    let metadata_routes_base = Router::new()
        .route("/zones", get(hierarchy::list_zones))
        .route("/zones/{zone_id}/stations", get(list_zone_stations))
        .route("/stations", get(hierarchy::list_stations))
        .route("/stations/{station_id}/sensors", get(list_station_sensors))
        .route("/sensors", get(hierarchy::list_sensors));

    let data_routes_base = Router::new()
        .route(
            "/stations/{station_id}/readings",
            get(readings::get_station_readings),
        )
        .route(
            "/stations/{station_id}/aggregates/{resolution}",
            get(aggregates::get_station_aggregates),
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
    let health_routes = Router::new().route("/healthz", get(health::healthz));

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

/// List stations belonging to a zone
#[utoipa::path(
    get,
    path = "/api/zones/{zone_id}/stations",
    params(
        ("zone_id" = String, Path, description = "Zone UUID or name"),
    ),
    responses(
        (status = 200, description = "Stations retrieved successfully", body = Vec<hierarchy::StationResponse>),
        (status = 404, description = "Zone not found"),
    ),
    tag = "hierarchy"
)]
async fn list_zone_stations(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> AppResult<Json<Vec<hierarchy::StationResponse>>> {
    let zone = resolve_zone(&state.db, &zone_id).await?;

    // Get stations for this zone
    let stations_list = stations::Entity::find()
        .filter(stations::Column::ZoneId.eq(zone.id))
        .order_by_asc(stations::Column::Name)
        .all(&state.db)
        .await?;

    let response: Vec<hierarchy::StationResponse> = stations_list
        .into_iter()
        .map(|s| hierarchy::StationResponse {
            id: s.id,
            zone_id: s.zone_id,
            name: s.name,
            latitude: s.latitude,
            longitude: s.longitude,
            altitude_m: s.altitude_m,
        })
        .collect();

    Ok(Json(response))
}

/// List sensors belonging to a station
#[utoipa::path(
    get,
    path = "/api/stations/{station_id}/sensors",
    params(
        ("station_id" = String, Path, description = "Station UUID or name"),
    ),
    responses(
        (status = 200, description = "Sensors retrieved successfully", body = Vec<hierarchy::SensorResponse>),
        (status = 404, description = "Station not found"),
    ),
    tag = "hierarchy"
)]
async fn list_station_sensors(
    State(state): State<AppState>,
    Path(station_id): Path<String>,
) -> AppResult<Json<Vec<hierarchy::SensorResponse>>> {
    let station = resolve_station(&state.db, &station_id).await?;

    // Get sensors for this station (active only by default)
    let sensors_list = sensors::Entity::find()
        .filter(sensors::Column::StationId.eq(station.id))
        .filter(sensors::Column::IsActive.eq(true))
        .order_by_asc(sensors::Column::Name)
        .all(&state.db)
        .await?;

    let response: Vec<hierarchy::SensorResponse> = sensors_list
        .into_iter()
        .map(|s| hierarchy::SensorResponse {
            id: s.id,
            station_id: s.station_id,
            name: s.name,
            sensor_type: s.sensor_type,
            display_units: s.display_units,
            sample_interval_sec: s.sample_interval_sec,
            is_active: s.is_active,
        })
        .collect();

    Ok(Json(response))
}
