use std::env;

#[derive(Debug, Clone)]
pub enum Deployment {
    Local,
    Dev,
    Stage,
    Prod,
}

impl std::str::FromStr for Deployment {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "dev" | "development" => Ok(Self::Dev),
            "stage" | "staging" => Ok(Self::Stage),
            "prod" | "production" => Ok(Self::Prod),
            "local" | "" => Ok(Self::Local),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    // Database
    pub database_url: String,

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

    // Keycloak authentication (all optional for gradual adoption)
    pub keycloak_url: Option<String>,
    pub keycloak_realm: Option<String>,
    pub keycloak_client_id: Option<String>,

    // Keycloak admin proxy (optional — enables user management)
    pub keycloak_admin_client_id: Option<String>,
    pub keycloak_admin_client_secret: Option<String>,

    // CORS
    pub cors_allowed_origins: Vec<String>,

    // Connection pool
    pub db_max_connections: u32,
    pub db_min_connections: u32,

    // Request timeout (seconds)
    pub request_timeout_seconds: u64,

    // Time range limits (days)
    pub max_readings_time_range_days: i64,
    pub max_aggregates_time_range_days: i64,
    pub public_max_readings_time_range_days: i64,
    pub public_max_aggregates_time_range_days: i64,
    pub default_readings_lookback_days: i64,

    // Derived-parameter janitor
    pub janitor_interval_seconds: u64,
    pub janitor_full_refresh_seconds: u64,
    pub janitor_retention_days: u32,
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
            // Database: prefer DATABASE_URL, fall back to individual DB_* vars
            database_url: env::var("DATABASE_URL")
                .or_else(|_| {
                    let user = env::var("DB_USER")?;
                    let password = env::var("DB_PASSWORD")?;
                    let host = env::var("DB_HOST")?;
                    let port = env::var("DB_PORT").unwrap_or_else(|_| "5432".to_string());
                    let name = env::var("DB_NAME")?;
                    Ok::<String, env::VarError>(format!(
                        "postgresql://{user}:{password}@{host}:{port}/{name}"
                    ))
                })
                .map_err(|_| {
                    ConfigError::Missing("DATABASE_URL or DB_USER/DB_PASSWORD/DB_HOST/DB_NAME")
                })?,

            // API settings
            api_host: env::var("API_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            api_port: env::var("API_PORT")
                .unwrap_or_else(|_| "3000".to_string())
                .parse()
                .unwrap_or(3000),

            // Rate limiting
            // With response caching, rate limits primarily prevent bandwidth abuse
            // rather than DB protection. Cache handles repeated queries efficiently.
            disable_rate_limiting: env::var("DISABLE_RATE_LIMITING")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),
            rate_limit_metadata_per_second: env::var("RATE_LIMIT_METADATA_PER_SECOND")
                .unwrap_or_else(|_| "50".to_string())
                .parse()
                .unwrap_or(50),
            rate_limit_metadata_burst: env::var("RATE_LIMIT_METADATA_BURST")
                .unwrap_or_else(|_| "200".to_string())
                .parse()
                .unwrap_or(200),
            rate_limit_data_per_second: env::var("RATE_LIMIT_DATA_PER_SECOND")
                .unwrap_or_else(|_| "100".to_string())
                .parse()
                .unwrap_or(100),
            rate_limit_data_burst: env::var("RATE_LIMIT_DATA_BURST")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .unwrap_or(300),
            bulk_concurrent_limit: env::var("BULK_CONCURRENT_LIMIT")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .unwrap_or(10),

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
            deployment: env::var("DEPLOYMENT")
                .unwrap_or_else(|_| "local".to_string())
                .parse()
                .unwrap_or(Deployment::Local),

            // Keycloak authentication (optional)
            keycloak_url: env::var("KEYCLOAK_URL").ok().filter(|s| !s.is_empty()),
            keycloak_realm: env::var("KEYCLOAK_REALM").ok().filter(|s| !s.is_empty()),
            keycloak_client_id: env::var("KEYCLOAK_CLIENT_ID")
                .ok()
                .filter(|s| !s.is_empty()),

            // Keycloak admin proxy (optional)
            keycloak_admin_client_id: env::var("KEYCLOAK_ADMIN_CLIENT_ID")
                .ok()
                .filter(|s| !s.is_empty()),
            keycloak_admin_client_secret: env::var("KEYCLOAK_ADMIN_CLIENT_SECRET")
                .ok()
                .filter(|s| !s.is_empty()),

            // CORS
            cors_allowed_origins: env::var("CORS_ALLOWED_ORIGINS")
                .unwrap_or_else(|_| "http://localhost:5173,http://localhost:3005".to_string())
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),

            // Connection pool
            db_max_connections: env::var("DB_MAX_CONNECTIONS")
                .unwrap_or_else(|_| "25".to_string())
                .parse()
                .unwrap_or(25),
            db_min_connections: env::var("DB_MIN_CONNECTIONS")
                .unwrap_or_else(|_| "5".to_string())
                .parse()
                .unwrap_or(5),

            // Request timeout
            request_timeout_seconds: env::var("REQUEST_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "60".to_string())
                .parse()
                .unwrap_or(60),

            // Time range limits
            max_readings_time_range_days: env::var("MAX_READINGS_TIME_RANGE_DAYS")
                .unwrap_or_else(|_| "90".to_string())
                .parse()
                .unwrap_or(90),
            max_aggregates_time_range_days: env::var("MAX_AGGREGATES_TIME_RANGE_DAYS")
                .unwrap_or_else(|_| "365".to_string())
                .parse()
                .unwrap_or(365),
            public_max_readings_time_range_days: env::var("PUBLIC_MAX_READINGS_TIME_RANGE_DAYS")
                .unwrap_or_else(|_| "30".to_string())
                .parse()
                .unwrap_or(30),
            public_max_aggregates_time_range_days: env::var("PUBLIC_MAX_AGGREGATES_TIME_RANGE_DAYS")
                .unwrap_or_else(|_| "180".to_string())
                .parse()
                .unwrap_or(180),
            default_readings_lookback_days: env::var("DEFAULT_READINGS_LOOKBACK_DAYS")
                .unwrap_or_else(|_| "7".to_string())
                .parse()
                .unwrap_or(7),

            // Derived-parameter janitor
            janitor_interval_seconds: env::var("JANITOR_INTERVAL_SECONDS")
                .unwrap_or_else(|_| "3600".to_string())
                .parse()
                .unwrap_or(3600),
            janitor_full_refresh_seconds: env::var("JANITOR_FULL_REFRESH_SECONDS")
                .unwrap_or_else(|_| "86400".to_string())
                .parse()
                .unwrap_or(86_400),
            janitor_retention_days: env::var("JANITOR_RETENTION_DAYS")
                .unwrap_or_else(|_| "180".to_string())
                .parse()
                .unwrap_or(180),
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
