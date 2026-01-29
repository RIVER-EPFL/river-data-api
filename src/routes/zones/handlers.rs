use axum::{
    extract::{Path, State},
    Json,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

use crate::common::AppState;
use crate::entity::{stations, zones};
use crate::error::AppResult;
use crate::routes::resolve_zone;
use crate::routes::stations::StationResponse;

use super::types::ZoneResponse;

/// List all zones
#[utoipa::path(
    get,
    path = "/api/zones",
    responses(
        (status = 200, description = "Zones retrieved successfully", body = Vec<ZoneResponse>),
    ),
    tag = "zones"
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

/// Get a specific zone by ID or name
#[utoipa::path(
    get,
    path = "/api/zones/{zone_id}",
    params(
        ("zone_id" = String, Path, description = "Zone UUID or name"),
    ),
    responses(
        (status = 200, description = "Zone retrieved successfully", body = ZoneResponse),
        (status = 404, description = "Zone not found"),
    ),
    tag = "zones"
)]
pub async fn get_zone(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> AppResult<Json<ZoneResponse>> {
    let zone = resolve_zone(&state.db, &zone_id).await?;

    Ok(Json(ZoneResponse {
        id: zone.id,
        name: zone.name,
        description: zone.description,
    }))
}

/// List stations belonging to a zone
#[utoipa::path(
    get,
    path = "/api/zones/{zone_id}/stations",
    params(
        ("zone_id" = String, Path, description = "Zone UUID or name"),
    ),
    responses(
        (status = 200, description = "Stations retrieved successfully", body = Vec<StationResponse>),
        (status = 404, description = "Zone not found"),
    ),
    tag = "zones"
)]
pub async fn list_zone_stations(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> AppResult<Json<Vec<StationResponse>>> {
    let zone = resolve_zone(&state.db, &zone_id).await?;

    let stations_list = stations::Entity::find()
        .filter(stations::Column::ZoneId.eq(zone.id))
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
