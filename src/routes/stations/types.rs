use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// Brief zone reference for embedding in responses
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ZoneRef {
    pub id: Uuid,
    pub name: String,
}

/// Brief station reference for embedding in responses
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StationRef {
    pub id: Uuid,
    pub name: String,
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

/// Sensor information embedded in station responses
#[derive(Debug, Serialize, ToSchema)]
pub struct SensorResponse {
    pub id: Uuid,
    pub name: String,
    pub sensor_type: String,
    pub display_units: Option<String>,
    pub sample_interval_sec: Option<i32>,
    pub is_active: Option<bool>,
}

/// Detailed station response with zone info, sensors, and data range
#[derive(Debug, Serialize, ToSchema)]
pub struct StationDetailResponse {
    pub id: Uuid,
    pub name: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub altitude_m: Option<f64>,
    pub zone: Option<ZoneRef>,
    pub sensors: Vec<SensorResponse>,
    /// Earliest reading timestamp for this station
    pub data_start: Option<DateTime<Utc>>,
    /// Latest reading timestamp for this station
    pub data_end: Option<DateTime<Utc>>,
    /// Total number of readings for this station
    pub reading_count: i64,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct StationsQuery {
    /// Filter by zone ID
    pub zone_id: Option<Uuid>,
}
