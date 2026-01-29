use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// Alarm response
#[derive(Debug, Serialize, ToSchema)]
pub struct AlarmResponse {
    pub id: Uuid,
    pub vaisala_alarm_id: i32,
    pub severity: i16,
    pub description: String,
    pub error_text: Option<String>,
    pub alarm_type: Option<String>,
    pub when_on: DateTime<Utc>,
    pub when_off: Option<DateTime<Utc>>,
    pub when_ack: Option<DateTime<Utc>>,
    pub duration_sec: Option<f64>,
    pub status: bool,
    pub is_system: bool,
    pub serial_number: Option<String>,
    pub location_text: Option<String>,
    pub zone_text: Option<String>,
    pub station_id: Option<Uuid>,
    pub ack_required: bool,
    /// Sensor IDs associated with this alarm
    pub sensor_ids: Vec<Uuid>,
}

/// Brief alarm response for list views
#[derive(Debug, Serialize, ToSchema)]
pub struct AlarmSummary {
    pub id: Uuid,
    pub severity: i16,
    pub description: String,
    pub when_on: DateTime<Utc>,
    pub when_off: Option<DateTime<Utc>>,
    pub status: bool,
    pub is_system: bool,
    pub location_text: Option<String>,
    pub station_id: Option<Uuid>,
    /// Human-readable duration
    pub duration: String,
}

/// Event response
#[derive(Debug, Serialize, ToSchema)]
pub struct EventResponse {
    pub time: DateTime<Utc>,
    pub vaisala_event_num: i32,
    pub category: String,
    pub message: String,
    pub user_name: Option<String>,
    pub entity: Option<String>,
    pub entity_id: Option<i32>,
    pub sensor_id: Option<Uuid>,
    pub station_id: Option<Uuid>,
    pub device_id: Option<i32>,
}

/// Query parameters for alarms endpoint
#[derive(Debug, Deserialize, IntoParams)]
pub struct AlarmsQuery {
    /// Filter by active status
    pub active: Option<bool>,
    /// Filter by station ID (UUID or name)
    pub station_id: Option<String>,
    /// Filter by severity (0-2)
    pub severity: Option<i16>,
    /// Start of time range (ISO 8601)
    pub start: Option<DateTime<Utc>>,
    /// End of time range (ISO 8601)
    pub end: Option<DateTime<Utc>>,
}

/// Query parameters for events endpoint
#[derive(Debug, Deserialize, IntoParams)]
pub struct EventsQuery {
    /// Start of time range (ISO 8601) - required
    pub start: DateTime<Utc>,
    /// End of time range (ISO 8601) - required
    pub end: DateTime<Utc>,
    /// Filter by category (system, admin, alarm, transfer)
    pub category: Option<String>,
    /// Filter by station ID (UUID or name)
    pub station_id: Option<String>,
    /// Page number (1-indexed)
    #[serde(default = "default_page")]
    pub page: i32,
    /// Page size (max 1000)
    #[serde(default = "default_page_size")]
    pub page_size: i32,
}

fn default_page() -> i32 {
    1
}

fn default_page_size() -> i32 {
    100
}

/// Paginated events response
#[derive(Debug, Serialize, ToSchema)]
pub struct EventsListResponse {
    pub events: Vec<EventResponse>,
    pub total: i64,
    pub page: i32,
    pub page_size: i32,
}
