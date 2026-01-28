use std::env;

#[derive(Debug, Clone)]
pub enum Deployment {
    Local,
    Dev,
    Stage,
    Prod,
}

impl Deployment {
    #[must_use]
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "dev" | "development" => Self::Dev,
            "stage" | "staging" => Self::Stage,
            "prod" | "production" => Self::Prod,
            _ => Self::Local,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    // Database
    pub database_url: String,

    // Vaisala API
    pub vaisala_base_url: String,
    pub vaisala_bearer_token: String,
    pub vaisala_skip_tls_verify: bool,
    pub vaisala_max_history_days: i64,

    // Sync settings
    pub sync_readings_interval_seconds: u64,
    pub sync_device_status_interval_seconds: u64,
    pub sync_retry_max: u32,
    pub sync_retry_delay_seconds: u64,

    // API settings
    pub api_host: String,
    pub api_port: u16,

    // Rate limiting
    pub disable_rate_limiting: bool,
    pub rate_limit_metadata_per_second: u64,
    pub rate_limit_metadata_burst: u32,
    pub rate_limit_data_per_second: u64,
    pub rate_limit_data_burst: u32,
    pub bulk_concurrent_limit: usize,

    // Caching
    pub cache_ttl_seconds: u64,
    pub cache_max_bytes: u64,

    // Application metadata
    pub deployment: Deployment,
}

impl Config {
    /// Load configuration from environment variables.
    ///
    /// # Errors
    ///
    /// Returns `ConfigError::Missing` if required environment variables are not set.
    pub fn from_env() -> Result<Self, ConfigError> {
        dotenvy::dotenv().ok();

        Ok(Self {
            // Database
            database_url: env::var("DATABASE_URL")
                .map_err(|_| ConfigError::Missing("DATABASE_URL"))?,

            // Vaisala API
            vaisala_base_url: env::var("VAISALA_BASE_URL")
                .unwrap_or_else(|_| "https://your-vaisala-server.local/rest/v1".to_string()),
            vaisala_bearer_token: env::var("VAISALA_BEARER_TOKEN")
                .map_err(|_| ConfigError::Missing("VAISALA_BEARER_TOKEN"))?,
            vaisala_skip_tls_verify: env::var("VAISALA_SKIP_TLS_VERIFY")
                .unwrap_or_else(|_| "true".to_string())
                .parse()
                .unwrap_or(true),
            vaisala_max_history_days: env::var("VAISALA_MAX_HISTORY_DAYS")
                .unwrap_or_else(|_| "90".to_string())
                .parse()
                .unwrap_or(90),

            // Sync settings
            sync_readings_interval_seconds: env::var("SYNC_READINGS_INTERVAL_SECONDS")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .unwrap_or(300),
            sync_device_status_interval_seconds: env::var("SYNC_DEVICE_STATUS_INTERVAL_SECONDS")
                .unwrap_or_else(|_| "1800".to_string())
                .parse()
                .unwrap_or(1800),
            sync_retry_max: env::var("SYNC_RETRY_MAX")
                .unwrap_or_else(|_| "3".to_string())
                .parse()
                .unwrap_or(3),
            sync_retry_delay_seconds: env::var("SYNC_RETRY_DELAY_SECONDS")
                .unwrap_or_else(|_| "60".to_string())
                .parse()
                .unwrap_or(60),

            // API settings
            api_host: env::var("API_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            api_port: env::var("API_PORT")
                .unwrap_or_else(|_| "3000".to_string())
                .parse()
                .unwrap_or(3000),

            // Rate limiting
            disable_rate_limiting: env::var("DISABLE_RATE_LIMITING")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),
            rate_limit_metadata_per_second: env::var("RATE_LIMIT_METADATA_PER_SECOND")
                .unwrap_or_else(|_| "1".to_string())
                .parse()
                .unwrap_or(1),
            rate_limit_metadata_burst: env::var("RATE_LIMIT_METADATA_BURST")
                .unwrap_or_else(|_| "60".to_string())
                .parse()
                .unwrap_or(60),
            rate_limit_data_per_second: env::var("RATE_LIMIT_DATA_PER_SECOND")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .unwrap_or(10),
            rate_limit_data_burst: env::var("RATE_LIMIT_DATA_BURST")
                .unwrap_or_else(|_| "60".to_string())
                .parse()
                .unwrap_or(60),
            bulk_concurrent_limit: env::var("BULK_CONCURRENT_LIMIT")
                .unwrap_or_else(|_| "5".to_string())
                .parse()
                .unwrap_or(5),

            // Caching
            cache_ttl_seconds: env::var("CACHE_TTL_SECONDS")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .unwrap_or(300), // 5 minutes default
            cache_max_bytes: env::var("CACHE_MAX_BYTES")
                .unwrap_or_else(|_| "209715200".to_string())
                .parse()
                .unwrap_or(209_715_200), // 200MB default

            // Application metadata
            deployment: Deployment::from_str(
                &env::var("DEPLOYMENT").unwrap_or_else(|_| "local".to_string()),
            ),
        })
    }

    #[must_use]
    pub fn bind_address(&self) -> String {
        format!("{}:{}", self.api_host, self.api_port)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Missing required environment variable: {0}")]
    Missing(&'static str),
}
