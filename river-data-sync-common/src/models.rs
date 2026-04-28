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
            instance_id: std::env::var("INSTANCE_ID").unwrap_or_else(|_| "default".to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enroll_request_serialization() {
        let req = EnrollRequest {
            client_id: "svc_abc".to_string(),
            client_secret: "secret123".to_string(),
            instance_id: "vaisala-01".to_string(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["client_id"], "svc_abc");
        assert_eq!(json["instance_id"], "vaisala-01");
    }

    #[test]
    fn test_enroll_response_deserialization() {
        let json = serde_json::json!({
            "service_id": "550e8400-e29b-41d4-a716-446655440000",
            "session_token": "tok-abc"
        });
        let resp: EnrollResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.session_token, "tok-abc");
    }

    #[test]
    fn test_heartbeat_response_with_commands() {
        let json = serde_json::json!({
            "session_token": "new-tok",
            "pending_commands": [
                {
                    "id": "550e8400-e29b-41d4-a716-446655440000",
                    "command": "trigger_sync",
                    "payload": null
                }
            ]
        });
        let resp: HeartbeatResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.pending_commands.len(), 1);
        assert_eq!(resp.pending_commands[0].command, "trigger_sync");
    }

    #[test]
    fn test_heartbeat_response_empty_commands() {
        let json = serde_json::json!({
            "session_token": "tok",
            "pending_commands": []
        });
        let resp: HeartbeatResponse = serde_json::from_value(json).unwrap();
        assert!(resp.pending_commands.is_empty());
    }

    #[test]
    fn test_sync_result_default() {
        let r = SyncResult::default();
        assert_eq!(r.readings_synced, 0);
        assert_eq!(r.status_events_synced, 0);
        assert!(!r.full_sync);
        assert!(r.errors.is_empty());
    }

    #[test]
    fn test_sync_result_serialization_skips_empty() {
        let r = SyncResult {
            readings_synced: 100,
            ..Default::default()
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["readings_synced"], 100);
        assert!(json.get("errors").is_none()); // skipped because empty
    }

    #[test]
    fn test_ingest_reading_serialization() {
        let r = IngestReading {
            time: chrono::Utc::now(),
            raw_value: 42.5,
            replicate_index: 0,
            sensor_id: None,
            calibration_id: None,
            deployment_id: None,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["raw_value"], 42.5);
        // replicate_index=0 should be skipped
        assert!(json.get("replicate_index").is_none());
        // sensor_id=None should be skipped
        assert!(json.get("sensor_id").is_none());
    }

    #[test]
    fn test_ingest_reading_with_replicate() {
        let r = IngestReading {
            time: chrono::Utc::now(),
            raw_value: 1.0,
            replicate_index: 2,
            sensor_id: None,
            calibration_id: None,
            deployment_id: None,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["replicate_index"], 2);
    }

    #[test]
    fn test_register_stream_request() {
        let req = RegisterStreamRequest {
            source_system: "vaisala".to_string(),
            source_key: "loc_1270".to_string(),
            source_name: Some("MDepthmm".to_string()),
            source_path: None,
            metadata: serde_json::json!({"device": "25284027"}),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["source_system"], "vaisala");
        assert_eq!(json["metadata"]["device"], "25284027");
    }

    #[test]
    fn test_data_stream_deserialization() {
        let json = serde_json::json!({
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "source_system": "vaisala",
            "source_key": "loc_1270",
            "source_name": "MDepthmm",
            "source_path": null,
            "metadata": {},
            "site_parameter_id": null,
            "is_active": true,
            "last_data_time": null
        });
        let stream: DataStream = serde_json::from_value(json).unwrap();
        assert_eq!(stream.source_system, "vaisala");
        assert!(stream.is_active);
        assert!(stream.site_parameter_id.is_none());
    }
}

// ============================================================================
// River Data API types (shared across sync services)
// ============================================================================

/// Data stream from the API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataStream {
    pub id: Uuid,
    pub source_system: String,
    pub source_key: String,
    pub source_name: Option<String>,
    pub source_path: Option<String>,
    pub metadata: serde_json::Value,
    pub site_parameter_id: Option<Uuid>,
    pub is_active: bool,
    pub last_data_time: Option<chrono::DateTime<chrono::Utc>>,
}

/// Stream registration request
#[derive(Debug, Serialize)]
pub struct RegisterStreamRequest {
    pub source_system: String,
    pub source_key: String,
    pub source_name: Option<String>,
    pub source_path: Option<String>,
    pub metadata: serde_json::Value,
}

/// Ingest readings request
#[derive(Debug, Serialize)]
pub struct IngestReadingsRequest {
    pub stream_id: Uuid,
    pub readings: Vec<IngestReading>,
}

/// Single reading for ingest
#[derive(Debug, Clone, Serialize)]
pub struct IngestReading {
    pub time: chrono::DateTime<chrono::Utc>,
    pub raw_value: f64,
    #[serde(skip_serializing_if = "is_zero")]
    pub replicate_index: i16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sensor_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibration_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<Uuid>,
}

fn is_zero(v: &i16) -> bool {
    *v == 0
}

/// Ingest status events request
#[derive(Debug, Serialize)]
pub struct IngestStatusEventsRequest {
    pub stream_id: Uuid,
    pub events: Vec<IngestStatusEvent>,
}

/// Single status event for ingest
#[derive(Debug, Serialize)]
pub struct IngestStatusEvent {
    pub time: chrono::DateTime<chrono::Utc>,
    pub value: String,
}
