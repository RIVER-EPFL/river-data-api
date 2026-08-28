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

    // Rate limiting (public API only; authenticated tier is not rate-limited)
    pub disable_rate_limiting: bool,
    pub bulk_concurrent_limit: usize,

    // Caching
    pub cache_ttl_seconds: u64,
    // API-token validation cache TTL (seconds). Short so revocations/expiries take effect quickly;
    // revoke/rotate also bust the cache explicitly.
    pub token_cache_ttl_seconds: u64,
    pub grants_cache_ttl_seconds: u64,
    pub cache_max_bytes: u64,

    // Application metadata
    pub deployment: Deployment,

    // Keycloak authentication (all optional for gradual adoption)
    pub keycloak_url: Option<String>,
    pub keycloak_realm: Option<String>,
    pub keycloak_client_id: Option<String>,

    // Keycloak admin proxy (optional, enables user management)
    pub keycloak_admin_client_id: Option<String>,
    pub keycloak_admin_client_secret: Option<String>,

    // CORS
    pub cors_allowed_origins: Vec<String>,

    // Connection pool
    pub db_max_connections: u32,
    pub db_min_connections: u32,

    // Request timeout (seconds)
    pub request_timeout_seconds: u64,

    // Default lookback when no start time is provided (days)
    pub default_readings_lookback_days: i64,

    // Public API rate limit (token bucket: burst_size cells, refilled 1 per period)
    pub public_rate_limit_burst: u32,
    pub public_rate_limit_period_secs: u64,

    // Authenticated-tier per-IP rate limit, in true requests/second (+ burst). Generous by design,
    // auth is still the primary gate; this only bounds an auth-failure / argon2 flood from a single
    // client IP, and real loggers push infrequent large batches well under it. Honors
    // DISABLE_RATE_LIMITING.
    pub auth_rate_limit_per_second: u64,
    pub auth_rate_limit_burst: u32,

    // Forensic audit log of API-token use (token_id/method/path/status per request). On by default
    // for the public-facing key surface; best-effort and fire-and-forget so it never blocks a request.
    pub audit_api_token_use: bool,

    /// How often served-request volume is summarised into one log line. 0 logs no summary, leaving
    /// only the DEBUG line each request already writes.
    pub request_summary_seconds: u64,

    /// Base URL of the R tool runner (OpenCPU), e.g. `http://river-tools-r/ocpu`. Unset leaves
    /// the tool endpoints answering 503.
    pub tools_runner_url: Option<String>,
    pub tools_runner_timeout_seconds: u64,

    // Derived-parameter janitor
    pub janitor_interval_seconds: u64,
    pub janitor_full_refresh_seconds: u64,
    /// Retention for `operator`/`metadata` tracked jobs (audit value). 0 disables.
    pub janitor_retention_days: u32,
    /// Retention for high-volume `maintenance` tracked jobs (janitor/ingest/refresh/alarm backfill).
    /// Much shorter than operator/metadata. 0 disables age-based pruning. See also the count cap.
    pub job_maintenance_retention_days: u32,
    /// Hard cap on retained `maintenance` job rows regardless of age, a burst can't blow storage
    /// between daily prunes. 0 disables the cap.
    pub job_maintenance_max_rows: u64,

    // Alarm sweeper: how often to reconcile persisted alarm_events against current breaches.
    // The sweep is only the backstop, ingest, config changes, and job completions reconcile
    // event-driven (~1s), so this just bounds how long a missed trigger can stay stale.
    pub alarm_sweep_interval_seconds: u64,

    // Sync-event sweeper: closes sync_events rows left 'running' by a sync service
    // that died mid-cycle (nothing client-side can terminate them).
    pub sync_event_sweep_interval_seconds: u64,
    pub sync_event_stale_after_seconds: u64,

    // Sync control plane: session token lifetime, how long an unacknowledged command stays
    // deliverable, the heartbeat-age thresholds behind a service's health, and the prefix on a
    // minted enrollment client_id. Services in the field hold tokens issued under these values.
    pub sync_session_token_ttl_secs: u64,
    pub sync_command_expiry_secs: u64,
    pub sync_health_healthy_secs: i64,
    pub sync_health_warning_secs: i64,
    pub sync_client_id_prefix: String,

    // Tracked-job retry policy (calibration/deployment/derived reprocessing, aggregate refresh, ...)
    pub job_max_retries: u32,
    pub job_retry_backoff_seconds: u64,

    // Notifications. Web Push is the only channel; dispatcher poll cadence (the broadcast wakeup is
    // primary; this bounds a missed event) and the push subscription reconciliation sweep cadence.
    pub notify_poll_interval_seconds: u64,
    pub identity_reconcile_interval_seconds: u64,
    // How often the background health probe checks each configured channel (getMe / SMTP NOOP /
    // Graph token) and upserts notification_channel_health.
    pub notify_health_interval_seconds: u64,
    // Battery depletion forecast: cutoff voltage and the days-to-cutoff threshold that raises an alert.
    pub battery_cutoff_volts: f64,
    pub battery_forecast_alert_days: i64,
    // A paired slot with no reading newer than this many hours raises a stale-data alert.
    pub stale_data_threshold_hours: i64,
    // Dashboard base URL used to build deep links in notification messages.
    pub dashboard_base_url: Option<String>,

    // Web Push (VAPID). The private key PEM signs each push; the public key goes to browsers at
    // subscription time; the subject is a mailto: or https: contact for the push service operator.
    pub vapid_private_key_pem: Option<String>,
    pub vapid_public_key: Option<String>,
    pub vapid_subject: Option<String>,
}

impl Config {
    /// Whether Web Push is configured (a VAPID private key and subject are present).
    #[must_use]
    pub fn web_push_configured(&self) -> bool {
        self.vapid_private_key_pem.is_some() && self.vapid_subject.is_some()
    }

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

            disable_rate_limiting: env::var("DISABLE_RATE_LIMITING")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),
            bulk_concurrent_limit: env::var("BULK_CONCURRENT_LIMIT")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .unwrap_or(10),

            // Caching
            cache_ttl_seconds: env::var("CACHE_TTL_SECONDS")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .unwrap_or(300), // 5 minutes default
            token_cache_ttl_seconds: env::var("TOKEN_CACHE_TTL_SECONDS")
                .unwrap_or_else(|_| "5".to_string())
                .parse()
                .unwrap_or(5), // 5s default, tight revocation/expiry window, negligible DB load
            // (expiry is re-checked every request; revoke/rotate bust the cache)
            grants_cache_ttl_seconds: env::var("GRANTS_CACHE_TTL_SECONDS")
                .unwrap_or_else(|_| "30".to_string())
                .parse()
                .unwrap_or(30), // per-user project grants; grant mutations bust the cache directly
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

            default_readings_lookback_days: env::var("DEFAULT_READINGS_LOOKBACK_DAYS")
                .unwrap_or_else(|_| "7".to_string())
                .parse()
                .unwrap_or(7),

            // Public API rate limit, modest by design; responses are cache-backed.
            // Defaults: burst 10, 1 token per 2s ⇒ ~30/min sustained.
            public_rate_limit_burst: env::var("PUBLIC_RATE_LIMIT_BURST")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .unwrap_or(10),
            public_rate_limit_period_secs: env::var("PUBLIC_RATE_LIMIT_PERIOD_SECS")
                .unwrap_or_else(|_| "2".to_string())
                .parse()
                .unwrap_or(2),

            auth_rate_limit_per_second: env::var("AUTH_RATE_LIMIT_PER_SECOND")
                .unwrap_or_else(|_| "100".to_string())
                .parse()
                .unwrap_or(100),
            auth_rate_limit_burst: env::var("AUTH_RATE_LIMIT_BURST")
                .unwrap_or_else(|_| "200".to_string())
                .parse()
                .unwrap_or(200),

            audit_api_token_use: env::var("AUDIT_API_TOKEN_USE")
                .unwrap_or_else(|_| "true".to_string())
                .parse()
                .unwrap_or(true),

            request_summary_seconds: env::var("REQUEST_SUMMARY_SECONDS")
                .unwrap_or_else(|_| "30".to_string())
                .parse()
                .unwrap_or(30),

            tools_runner_url: env::var("TOOLS_RUNNER_URL")
                .ok()
                .map(|u| u.trim_end_matches('/').to_string())
                .filter(|u| !u.is_empty()),
            tools_runner_timeout_seconds: env::var("TOOLS_RUNNER_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "60".to_string())
                .parse()
                .unwrap_or(60),

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
            job_maintenance_retention_days: env::var("JOB_MAINTENANCE_RETENTION_DAYS")
                .unwrap_or_else(|_| "14".to_string())
                .parse()
                .unwrap_or(14),
            job_maintenance_max_rows: env::var("JOB_MAINTENANCE_MAX_ROWS")
                .unwrap_or_else(|_| "50000".to_string())
                .parse()
                .unwrap_or(50_000),

            alarm_sweep_interval_seconds: env::var("ALARM_SWEEP_INTERVAL_SECONDS")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .unwrap_or(300),

            sync_event_sweep_interval_seconds: env::var("SYNC_EVENT_SWEEP_INTERVAL_SECONDS")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .unwrap_or(300),
            sync_event_stale_after_seconds: env::var("SYNC_EVENT_STALE_AFTER_SECONDS")
                .unwrap_or_else(|_| "3600".to_string())
                .parse()
                .unwrap_or(3600),

            sync_session_token_ttl_secs: env::var("SYNC_SESSION_TOKEN_TTL_SECS")
                .unwrap_or_else(|_| "900".to_string())
                .parse()
                .unwrap_or(900),
            sync_command_expiry_secs: env::var("SYNC_COMMAND_EXPIRY_SECS")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .unwrap_or(300),
            sync_health_healthy_secs: env::var("SYNC_HEALTH_HEALTHY_SECS")
                .unwrap_or_else(|_| "90".to_string())
                .parse()
                .unwrap_or(90),
            sync_health_warning_secs: env::var("SYNC_HEALTH_WARNING_SECS")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .unwrap_or(300),
            sync_client_id_prefix: env::var("SYNC_CLIENT_ID_PREFIX")
                .unwrap_or_else(|_| "svc_".to_string()),

            job_max_retries: env::var("JOB_MAX_RETRIES")
                .unwrap_or_else(|_| "3".to_string())
                .parse()
                .unwrap_or(3),
            job_retry_backoff_seconds: env::var("JOB_RETRY_BACKOFF_SECONDS")
                .unwrap_or_else(|_| "60".to_string())
                .parse()
                .unwrap_or(60),

            notify_poll_interval_seconds: env::var("NOTIFY_POLL_INTERVAL_SECONDS")
                .unwrap_or_else(|_| "60".to_string())
                .parse()
                .unwrap_or(60),
            identity_reconcile_interval_seconds: env::var("IDENTITY_RECONCILE_INTERVAL_SECONDS")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .unwrap_or(300),
            notify_health_interval_seconds: env::var("NOTIFY_HEALTH_INTERVAL_SECONDS")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .unwrap_or(300),
            battery_cutoff_volts: env::var("BATTERY_CUTOFF_VOLTS")
                .unwrap_or_else(|_| "10.5".to_string())
                .parse()
                .unwrap_or(10.5),
            battery_forecast_alert_days: env::var("BATTERY_FORECAST_ALERT_DAYS")
                .unwrap_or_else(|_| "14".to_string())
                .parse()
                .unwrap_or(14),
            stale_data_threshold_hours: env::var("STALE_DATA_THRESHOLD_HOURS")
                .unwrap_or_else(|_| "6".to_string())
                .parse()
                .unwrap_or(6),
            dashboard_base_url: env::var("DASHBOARD_BASE_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            vapid_private_key_pem: env::var("VAPID_PRIVATE_KEY_PEM")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|s| s.replace("\\n", "\n")),
            vapid_public_key: env::var("VAPID_PUBLIC_KEY")
                .ok()
                .filter(|s| !s.is_empty()),
            vapid_subject: env::var("VAPID_SUBJECT")
                .ok()
                .filter(|s| !s.is_empty()),
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
