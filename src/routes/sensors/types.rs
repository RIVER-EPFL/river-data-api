use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

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
pub struct SensorsQuery {
    /// Filter by station ID
    pub station_id: Option<Uuid>,
    /// Filter by sensor type
    pub sensor_type: Option<String>,
    /// Include inactive sensors (default: false)
    #[serde(default)]
    pub include_inactive: bool,
}
