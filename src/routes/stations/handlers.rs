use axum::{
    extract::{Path, Query, State},
    Json,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

use crate::common::AppState;
use crate::entity::{sensors, stations, zones};
use crate::error::AppResult;
use crate::routes::resolve_station;
use crate::routes::sensors::SensorResponse;

use super::types::{StationDetailResponse, StationResponse, StationsQuery, ZoneRef};

/// List all stations
#[utoipa::path(
    get,
    path = "/api/stations",
    params(StationsQuery),
    responses(
        (status = 200, description = "Stations retrieved successfully", body = Vec<StationResponse>),
    ),
    tag = "stations"
)]
pub async fn list_stations(
    State(state): State<AppState>,
    Query(query): Query<StationsQuery>,
) -> AppResult<Json<Vec<StationResponse>>> {
    let mut db_query = stations::Entity::find();

    if let Some(zone_id) = query.zone_id {
        db_query = db_query.filter(stations::Column::ZoneId.eq(zone_id));
    }

    let stations_list = db_query
        .order_by_asc(stations::Column::Name)
        .all(&state.db)
        .await?;

    let response: Vec<StationResponse> = stations_list
        .into_iter()
        .map(|s| StationResponse {
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

/// Get a specific station by ID or name
#[utoipa::path(
    get,
    path = "/api/stations/{station_id}",
    params(
        ("station_id" = String, Path, description = "Station UUID or name"),
    ),
    responses(
        (status = 200, description = "Station retrieved successfully", body = StationDetailResponse),
        (status = 404, description = "Station not found"),
    ),
    tag = "stations"
)]
pub async fn get_station(
    State(state): State<AppState>,
    Path(station_id): Path<String>,
) -> AppResult<Json<StationDetailResponse>> {
    let station = resolve_station(&state.db, &station_id).await?;

    // Fetch zone info if available
    let zone = if let Some(zone_id) = station.zone_id {
        zones::Entity::find_by_id(zone_id)
            .one(&state.db)
            .await?
            .map(|z| ZoneRef {
                id: z.id,
                name: z.name,
            })
    } else {
        None
    };

    Ok(Json(StationDetailResponse {
        id: station.id,
        name: station.name,
        latitude: station.latitude,
        longitude: station.longitude,
        altitude_m: station.altitude_m,
        zone,
    }))
}

/// List sensors belonging to a station
#[utoipa::path(
    get,
    path = "/api/stations/{station_id}/sensors",
    params(
        ("station_id" = String, Path, description = "Station UUID or name"),
    ),
    responses(
        (status = 200, description = "Sensors retrieved successfully", body = Vec<SensorResponse>),
        (status = 404, description = "Station not found"),
    ),
    tag = "stations"
)]
pub async fn list_station_sensors(
    State(state): State<AppState>,
    Path(station_id): Path<String>,
) -> AppResult<Json<Vec<SensorResponse>>> {
    let station = resolve_station(&state.db, &station_id).await?;

    let sensors_list = sensors::Entity::find()
        .filter(sensors::Column::StationId.eq(station.id))
        .filter(sensors::Column::IsActive.eq(true))
        .order_by_asc(sensors::Column::Name)
        .all(&state.db)
        .await?;

    let response: Vec<SensorResponse> = sensors_list
        .into_iter()
        .map(|s| SensorResponse {
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
