use serde::{Deserialize, Serialize};

/// JSON API wrapper for responses
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonApiResponse<T> {
    pub jsonapi: JsonApiVersion,
    pub data: Vec<JsonApiResource<T>>,
    #[serde(default)]
    pub links: Option<serde_json::Value>,
    #[serde(default)]
    pub meta: Option<serde_json::Value>,
}

/// JSON API wrapper for paginated responses with meta
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonApiResponseWithMeta<T> {
    pub jsonapi: JsonApiVersion,
    pub data: Vec<JsonApiResource<T>>,
    #[serde(default)]
    pub links: Option<serde_json::Value>,
    #[serde(default)]
    pub meta: Option<PaginationMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginationMeta {
    #[serde(default)]
    pub total_record_count: i32,
    #[serde(default)]
    pub page_record_count: i32,
    #[serde(default)]
    pub page_size: i32,
    #[serde(default)]
    pub page_number: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonApiVersion {
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonApiResource<T> {
    #[serde(rename = "type")]
    pub resource_type: String,
    pub id: String,
    pub attributes: T,
}

/// Response from `/rest/v1/locations_history`
pub type LocationsHistoryResponse = JsonApiResponse<LocationHistoryAttributes>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationHistoryAttributes {
    pub id: i32,
    pub name: String,
    pub zone: String,
    #[serde(default)]
    pub timestamp: Option<i64>,
    #[serde(default)]
    pub value: Option<f64>,
    #[serde(default)]
    pub current_units: Option<String>,
    #[serde(default)]
    pub display_units: Option<String>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub max_time: Option<i64>,
    #[serde(default)]
    pub avg: Option<f64>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub min_time: Option<i64>,
    #[serde(default)]
    pub seconds: Option<i64>,
    #[serde(default)]
    pub decimal_places: Option<i16>,
    #[serde(default)]
    #[serde(rename = "std")]
    pub std_dev: Option<f64>,
    /// Mean Kinetic Temperature - can be null, "N/A", or a float
    #[serde(default)]
    pub mkt: Option<serde_json::Value>,
    #[serde(default)]
    pub samples: Option<i32>,
    #[serde(default)]
    pub realtime_samples: Option<i32>,
    /// Data points as [timestamp, value, logged] tuples
    #[serde(default)]
    pub data_points: Vec<DataPoint>,
    /// Threshold configuration (usually empty)
    #[serde(default)]
    pub thresholds: Vec<serde_json::Value>,
}

/// A single data point: [timestamp_epoch, value, logged_bool]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "RawDataPoint")]
pub struct DataPoint {
    pub timestamp: i64,
    pub value: f64,
    pub logged: bool,
}

/// Raw representation for deserializing [number, number|null, bool] arrays
/// Timestamps can be floats like 1593038703.8, values can be null
#[derive(Debug, Clone, Deserialize)]
struct RawDataPoint(f64, Option<f64>, bool);

impl From<RawDataPoint> for DataPoint {
    fn from(raw: RawDataPoint) -> Self {
        Self {
            // Convert float timestamp to integer (truncate decimal)
            timestamp: raw.0 as i64,
            // Use 0.0 as default for null values
            value: raw.1.unwrap_or(0.0),
            logged: raw.2,
        }
    }
}

/// Response from `/rest/v1/locations`
pub type LocationsResponse = JsonApiResponse<LocationAttributes>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationAttributes {
    /// Zone type name (Folder, Cupboard, etc.)
    #[serde(default)]
    pub type_name: String,
    /// Description
    #[serde(default)]
    pub description: String,
    /// Full path (e.g., "viewLinc/BREATHE/Martigny")
    #[serde(default)]
    pub path: String,
    /// Name of the location/zone
    #[serde(default)]
    pub text: String,
    /// Vertical position in parent zone
    #[serde(default)]
    pub pos: i32,
    /// The viewLinc node ID
    #[serde(default)]
    pub node_id: i32,
    /// Whether alarming is paused
    #[serde(default)]
    pub pause: bool,
    /// True if this is a sensor (Location), false if Zone
    #[serde(default)]
    pub leaf: bool,
    /// Zone type numerical ID
    #[serde(default)]
    pub type_id: i32,
    /// 0=Root Zone, 2=Zone, 3=Location
    #[serde(default)]
    pub node_type: i32,
    /// Whether the location is deleted/deactivated
    #[serde(default)]
    pub deleted: bool,
}

/// Response from `/rest/v1/active_alarms`
pub type ActiveAlarmsResponse = JsonApiResponse<ActiveAlarmAttributes>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveAlarmAttributes {
    pub id: i32,
    pub severity: i16,
    #[serde(default)]
    pub description: String,
    /// Error text (e.g., "Device Historical Data Alarm")
    #[serde(default, rename = "err")]
    pub error_text: String,
    /// Unix epoch timestamp when alarm activated
    pub when_on: f64,
    /// Unix epoch timestamp when alarm deactivated (null if still active)
    #[serde(default)]
    pub when_off: Option<f64>,
    /// Unix epoch timestamp when acknowledged
    #[serde(default)]
    pub when_ack: Option<f64>,
    /// Unix epoch timestamp of the alarm condition
    #[serde(default)]
    pub when_condition: Option<f64>,
    /// Duration string (e.g., "2h 30m")
    #[serde(default)]
    pub duration: String,
    /// Duration in seconds
    #[serde(default)]
    pub duration_sec: f64,
    /// True if alarm is currently active
    #[serde(default)]
    pub status: bool,
    /// True if this is a system-level alarm
    #[serde(default)]
    pub is_system: bool,
    /// Device serial number
    #[serde(default)]
    pub serial_number: String,
    /// Location name (denormalized)
    #[serde(default)]
    pub location: String,
    /// Zone name (denormalized)
    #[serde(default)]
    pub zone: String,
    /// Vaisala location IDs affected by this alarm
    #[serde(default)]
    pub location_ids: Vec<i32>,
    /// Whether acknowledgment is required
    #[serde(default)]
    pub ack_required: bool,
    /// Acknowledgment comments
    #[serde(default)]
    pub ack_comments: Option<Vec<String>>,
    /// Action taken during acknowledgment
    #[serde(default)]
    pub ack_action_taken: Option<String>,
    /// Logger description
    #[serde(default)]
    pub logger_description: String,
}

/// Response from `/rest/v1/events`
pub type EventsResponse = JsonApiResponseWithMeta<EventAttributes>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventAttributes {
    /// Vaisala event number (unique identifier)
    pub num: i32,
    /// Event category (system, admin, alarm, transfer)
    #[serde(default)]
    pub category: String,
    /// Unix epoch timestamp
    pub timestamp: f64,
    /// Event message
    #[serde(default, rename = "msg")]
    pub message: String,
    /// User who triggered the event
    #[serde(default, rename = "user")]
    pub user_name: String,
    /// Entity type
    #[serde(default)]
    pub entity: String,
    /// Entity ID
    #[serde(default)]
    pub entity_id: i32,
    /// Location ID (can be int, string like "N/A", or null)
    #[serde(default)]
    pub location_id: Option<LocationIdValue>,
    /// Device ID
    #[serde(default)]
    pub device_id: Option<i32>,
    /// Channel ID
    #[serde(default)]
    pub channel_id: Option<i32>,
    /// Host ID
    #[serde(default)]
    pub host_id: Option<i32>,
    /// Affected location IDs (comma-separated string)
    #[serde(default)]
    pub affected_location_ids: Option<String>,
    /// Comments on the event
    #[serde(default)]
    pub comments: Vec<EventComment>,
    /// Extra fields
    #[serde(default)]
    pub extra_fields: Vec<serde_json::Value>,
}

/// Location ID can be an integer or a string like "N/A"
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LocationIdValue {
    Int(i32),
    String(String),
}

impl LocationIdValue {
    pub fn as_int(&self) -> Option<i32> {
        match self {
            Self::Int(i) => Some(*i),
            Self::String(_) => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventComment {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub timestamp: f64,
}

/// Response from `/rest/v1/locations_data`
pub type LocationsDataResponse = JsonApiResponse<LocationDataAttributes>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationDataAttributes {
    pub id: i32,
    #[serde(default)]
    pub zone: String,
    #[serde(default)]
    pub location_name: String,
    #[serde(default)]
    pub location_description: String,
    #[serde(default)]
    pub location_path: String,
    #[serde(default)]
    pub location_type: String,
    #[serde(default)]
    pub permission: i32,
    #[serde(default)]
    pub value: f64,
    #[serde(default)]
    pub decimal_places: i16,
    #[serde(default)]
    pub display_units: String,
    #[serde(default)]
    pub channel_id: i32,
    #[serde(default)]
    pub logger_id: i32,
    #[serde(default)]
    pub logger_description: String,
    #[serde(default)]
    pub logger_serial_number: String,
    #[serde(default)]
    pub probe_serial_number: String,
    #[serde(default)]
    pub sample_interval_sec: i32,
    #[serde(default)]
    pub chindex: i32,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub logger_device: String,
    #[serde(default)]
    pub timestamp: i64,
    #[serde(default)]
    pub device_status: String,
    #[serde(default)]
    pub deleted: i32,
    #[serde(default)]
    pub device_class: String,
    #[serde(default)]
    pub battery_level: i16,
    #[serde(default)]
    pub battery_state: i16,
    #[serde(default)]
    pub line_powered: i16,
    #[serde(default)]
    pub signal_quality: i16,
    #[serde(default)]
    pub unreachable: bool,
}
