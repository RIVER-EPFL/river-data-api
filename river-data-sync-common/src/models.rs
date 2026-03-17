use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ============================================================================
// Enrollment
// ============================================================================

#[derive(Debug, Serialize)]
pub struct EnrollRequest {
    pub client_id: String,
    pub client_secret: String,
    pub instance_id: String,
}

#[derive(Debug, Deserialize)]
pub struct EnrollResponse {
    pub service_id: Uuid,
    pub session_token: String,
}

// ============================================================================
// Heartbeat
// ============================================================================

#[derive(Debug, Serialize)]
pub struct HeartbeatRequest {
    pub service_id: Uuid,
    pub client_secret: String,
    pub status: String,
    pub current_operation: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HeartbeatResponse {
    pub session_token: String,
    pub pending_commands: Vec<PendingCommand>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PendingCommand {
    pub id: Uuid,
    pub command: String,
    pub payload: Option<serde_json::Value>,
}

// ============================================================================
// Command Updates
// ============================================================================

#[derive(Debug, Serialize)]
pub struct CommandUpdateRequest {
    pub status: String,
    pub result: Option<serde_json::Value>,
}

// ============================================================================
// Sync Result
// ============================================================================

#[derive(Debug, Default, Serialize)]
pub struct SyncResult {
    pub readings_synced: u64,
    pub status_events_synced: u64,
    pub full_sync: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub log: Vec<String>,
}

/// What triggered a sync cycle.
#[derive(Debug)]
pub enum SyncTrigger {
    Scheduled,
    Command { id: Uuid, full: bool },
}

// ============================================================================
// Runner Config
// ============================================================================

#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub api_base_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub instance_id: String,
    pub heartbeat_interval_secs: u64,
    pub sync_interval_secs: u64,
}

impl RunnerConfig {
    pub fn from_env() -> Result<Self, String> {
        Ok(Self {
            api_base_url: require_env("API_BASE_URL")?,
            client_id: require_env("SERVICE_CLIENT_ID")?,
            client_secret: require_env("SERVICE_CLIENT_SECRET")?,
            instance_id: std::env::var("INSTANCE_ID")
                .unwrap_or_else(|_| "default".to_string()),
            heartbeat_interval_secs: env_u64("HEARTBEAT_INTERVAL_SECONDS", 30),
            sync_interval_secs: env_u64("SYNC_INTERVAL_SECONDS", 300),
        })
    }
}

fn require_env(key: &str) -> Result<String, String> {
    std::env::var(key).map_err(|_| format!("Missing required env var: {key}"))
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ============================================================================
// River Data API types (shared across sync services)
// ============================================================================

/// Project from the API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub data_source: String,
    pub description: Option<String>,
}

/// Site from the API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Site {
    pub id: Uuid,
    pub project_id: Option<Uuid>,
    pub name: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub altitude_m: Option<f64>,
}

/// Global parameter from the API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Parameter {
    pub id: Uuid,
    pub name: String,
    pub display_name: String,
    pub default_units: String,
    pub category: String,
    pub data_type: String,
}

/// Site parameter from the API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteParameter {
    pub id: Uuid,
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub name: String,
    pub sensor_type: String,
    pub is_active: Option<bool>,
    pub is_derived: Option<bool>,
}

/// Source mapping from the API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceMapping {
    pub source_system: String,
    pub entity_type: String,
    pub source_key: String,
    pub entity_id: Uuid,
    pub source_name: Option<String>,
}

/// Sensor deployment from the API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorDeployment {
    pub id: Uuid,
    pub sensor_id: Uuid,
    pub site_id: Uuid,
    pub deployed_from: chrono::DateTime<chrono::Utc>,
    pub deployed_until: Option<chrono::DateTime<chrono::Utc>>,
    pub deployment_type: Option<String>,
}

/// Sensor calibration from the API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorCalibration {
    pub id: Uuid,
    pub sensor_id: Uuid,
    pub slope: f64,
    pub intercept: f64,
    pub valid_from: chrono::DateTime<chrono::Utc>,
}

/// Sync state from the API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncState {
    pub site_parameter_id: Uuid,
    pub last_data_time: Option<chrono::DateTime<chrono::Utc>>,
    pub last_sync_attempt: Option<chrono::DateTime<chrono::Utc>>,
    pub sync_status: Option<String>,
    pub error_message: Option<String>,
    pub retry_count: Option<i32>,
    pub last_full_sync: Option<chrono::DateTime<chrono::Utc>>,
}

/// Reading for batch insert
#[derive(Debug, Clone, Serialize)]
pub struct ReadingInput {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    pub raw_value: f64,
    pub calibrated_value: Option<f64>,
    pub sensor_id: Option<Uuid>,
    pub calibration_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
}

/// Status event for batch insert
#[derive(Debug, Serialize)]
pub struct StatusEventInput {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    pub value: String,
    pub sensor_id: Option<Uuid>,
}
