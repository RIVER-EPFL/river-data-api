use axum::{
    extract::{Query, State},
    Json,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::{sensors, stations, zones};
use crate::error::AppResult;

#[derive(Debug, Serialize, ToSchema)]
pub struct ZoneResponse {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StationResponse {
    pub id: Uuid,
    pub zone_id: Option<Uuid>,
    pub name: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub altitude_m: Option<f64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SensorResponse {
    pub id: Uuid,
    pub station_id: Uuid,
    pub name: String,
    pub sensor_type: String,
    pub display_units: Option<String>,
    pub sample_interval_sec: Option<i32>,
    pub is_active: Option<bool>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct StationsQuery {
    /// Filter by zone ID
    pub zone_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct SensorsQuery {
    /// Filter by station ID
    pub station_id: Option<Uuid>,
    /// Filter by sensor type
    pub sensor_type: Option<String>,
    /// Include inactive sensors (default: false)
    #[serde(default)]
    pub include_inactive: bool,
}

/// List all zones
#[utoipa::path(
    get,
    path = "/api/zones",
    responses(
        (status = 200, description = "Zones retrieved successfully", body = Vec<ZoneResponse>),
    ),
    tag = "hierarchy"
)]
pub async fn list_zones(State(state): State<AppState>) -> AppResult<Json<Vec<ZoneResponse>>> {
    let zones_list = zones::Entity::find()
        .order_by_asc(zones::Column::Name)
        .all(&state.db)
        .await?;

    let response: Vec<ZoneResponse> = zones_list
        .into_iter()
        .map(|z| ZoneResponse {
            id: z.id,
            name: z.name,
            description: z.description,
        })
        .collect();

    Ok(Json(response))
}

/// List all stations
#[utoipa::path(
    get,
    path = "/api/stations",
    params(StationsQuery),
    responses(
        (status = 200, description = "Stations retrieved successfully", body = Vec<StationResponse>),
    ),
    tag = "hierarchy"
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

/// List all sensors
#[utoipa::path(
    get,
    path = "/api/sensors",
    params(SensorsQuery),
    responses(
        (status = 200, description = "Sensors retrieved successfully", body = Vec<SensorResponse>),
    ),
    tag = "hierarchy"
)]
pub async fn list_sensors(
    State(state): State<AppState>,
    Query(query): Query<SensorsQuery>,
) -> AppResult<Json<Vec<SensorResponse>>> {
    let mut db_query = sensors::Entity::find();

    if let Some(station_id) = query.station_id {
        db_query = db_query.filter(sensors::Column::StationId.eq(station_id));
    }

    if let Some(ref sensor_type) = query.sensor_type {
        db_query = db_query.filter(sensors::Column::SensorType.eq(sensor_type));
    }

    if !query.include_inactive {
        db_query = db_query.filter(sensors::Column::IsActive.eq(true));
    }

    let sensors_list = db_query
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
