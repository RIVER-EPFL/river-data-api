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
