use axum::{
    extract::{Query, State},
    Json,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

use crate::common::AppState;
use crate::entity::sensors;
use crate::error::AppResult;

use super::types::{SensorResponse, SensorsQuery};

/// List all sensors
#[utoipa::path(
    get,
    path = "/api/sensors",
    params(SensorsQuery),
    responses(
        (status = 200, description = "Sensors retrieved successfully", body = Vec<SensorResponse>),
    ),
    tag = "sensors"
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
