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
