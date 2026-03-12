/// Configuration for the Vaisala sync microservice.
#[derive(Debug, Clone)]
pub struct SyncConfig {
    // River Data API connection
    pub api_base_url: String,
    pub api_token: String,

    // Vaisala viewLinc connection
    pub vaisala_base_url: String,
    pub vaisala_bearer_token: String,
    pub vaisala_skip_tls_verify: bool,

    // Sync intervals (seconds)
    pub sync_interval_seconds: u64,
    pub max_history_days: i64,

    // Retry configuration
    pub retry_delay_seconds: u64,
    pub retry_max: u32,
}

impl SyncConfig {
    pub fn from_env() -> Result<Self, String> {
        Ok(Self {
            api_base_url: require_env("API_BASE_URL")?,
            api_token: require_env("API_TOKEN")?,

            vaisala_base_url: require_env("VAISALA_BASE_URL")?,
            vaisala_bearer_token: require_env("VAISALA_BEARER_TOKEN")?,
            vaisala_skip_tls_verify: env_bool("VAISALA_SKIP_TLS_VERIFY", true),

            sync_interval_seconds: env_u64("SYNC_INTERVAL_SECONDS", 300),
            max_history_days: env_i64("MAX_HISTORY_DAYS", 90),

            retry_delay_seconds: env_u64("RETRY_DELAY_SECONDS", 60),
            retry_max: env_u32("RETRY_MAX", 3),
        })
    }
}

fn require_env(key: &str) -> Result<String, String> {
    std::env::var(key).map_err(|_| format!("Missing required env var: {key}"))
}

fn env_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .ok()
        .map(|v| v == "true" || v == "1")
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_i64(key: &str, default: i64) -> i64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
