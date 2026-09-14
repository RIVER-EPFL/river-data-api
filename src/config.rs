use river_data_core::env::{parse_or, string_or};
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
    /// Networks a forwarded-for chain may be believed from. Anything reaching the API from
    /// outside these is keyed on its own address, whatever headers it sends.
    pub trusted_proxy_cidrs: Vec<crate::common::rate_limit::IpCidr>,

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

    /// MeteoSwiss Open Government Data SMN collection. The per-station recent file hangs under it.
    pub meteoswiss_base_url: String,
    /// The all-stations latest-values file, republished every ten minutes.
    pub meteoswiss_latest_url: String,
    pub meteoswiss_interval_seconds: u64,
    pub meteoswiss_recent_interval_seconds: u64,
    pub meteoswiss_timeout_seconds: u64,

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
    /// Age-based retention for terminal sync_events rows (days); 0 disables pruning.
    pub sync_event_retention_days: u32,
    /// Age-based retention for ingest_receipts rows (days); 0 disables pruning.
    pub ingest_receipt_retention_days: u32,

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
    // Graph token) and records each channel's health in notification_state.
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
    /// The configuration a router built only to describe itself runs under.
    ///
    /// Nothing is served and no query is issued, so every value is a placeholder and none of them
    /// reaches the document. The Keycloak admin client is declared because the user-management
    /// routes mount only when one is present, and reading none of this from the environment is
    /// what lets the guard in `routes` build the same document the dump writes.
    #[must_use]
    pub fn for_openapi_document() -> Self {
        Self {
            database_url: String::new(),
            api_host: "127.0.0.1".to_string(),
            api_port: 0,
            disable_rate_limiting: true,
            bulk_concurrent_limit: 10,
            cache_ttl_seconds: 0,
            token_cache_ttl_seconds: 0,
            grants_cache_ttl_seconds: 0,
            cache_max_bytes: 0,
            deployment: Deployment::Local,
            keycloak_url: None,
            keycloak_realm: None,
            keycloak_client_id: None,
            keycloak_admin_client_id: Some("river-data-admin".to_string()),
            keycloak_admin_client_secret: Some("unused".to_string()),
            cors_allowed_origins: Vec::new(),
            trusted_proxy_cidrs: crate::common::rate_limit::default_trusted_proxies(),
            db_max_connections: 1,
            db_min_connections: 0,
            request_timeout_seconds: 60,
            default_readings_lookback_days: 7,
            public_rate_limit_burst: 10,
            public_rate_limit_period_secs: 2,
            auth_rate_limit_per_second: 100,
            auth_rate_limit_burst: 200,
            audit_api_token_use: false,
            request_summary_seconds: 0,
            tools_runner_url: None,
            tools_runner_timeout_seconds: 60,
            meteoswiss_base_url: String::new(),
            meteoswiss_latest_url: String::new(),
            meteoswiss_interval_seconds: 600,
            meteoswiss_recent_interval_seconds: 86_400,
            meteoswiss_timeout_seconds: 30,
            janitor_interval_seconds: 3600,
            janitor_full_refresh_seconds: 86_400,
            janitor_retention_days: 180,
            job_maintenance_retention_days: 14,
            job_maintenance_max_rows: 50_000,
            alarm_sweep_interval_seconds: 60,
            sync_event_sweep_interval_seconds: 300,
            sync_event_stale_after_seconds: 3600,
            sync_event_retention_days: 90,
            ingest_receipt_retention_days: 365,
            sync_session_token_ttl_secs: 900,
            sync_command_expiry_secs: 300,
            sync_health_healthy_secs: 90,
            sync_health_warning_secs: 300,
            sync_client_id_prefix: "svc_".to_string(),
            job_max_retries: 3,
            job_retry_backoff_seconds: 60,
            notify_poll_interval_seconds: 60,
            identity_reconcile_interval_seconds: 300,
            notify_health_interval_seconds: 300,
            battery_cutoff_volts: 10.5,
            battery_forecast_alert_days: 14,
            stale_data_threshold_hours: 6,
            dashboard_base_url: None,
            vapid_private_key_pem: None,
            vapid_public_key: None,
            vapid_subject: None,
        }
    }

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
            api_host: string_or("API_HOST", "0.0.0.0"),
            api_port: parse_or("API_PORT", 3000),

            disable_rate_limiting: parse_or("DISABLE_RATE_LIMITING", false),
            bulk_concurrent_limit: parse_or("BULK_CONCURRENT_LIMIT", 10),

            // Caching
            cache_ttl_seconds: parse_or("CACHE_TTL_SECONDS", 300), // 5 minutes default
            token_cache_ttl_seconds: parse_or("TOKEN_CACHE_TTL_SECONDS", 5), // 5s default, tight revocation/expiry window, negligible DB load
            // (expiry is re-checked every request; revoke/rotate bust the cache)
            grants_cache_ttl_seconds: parse_or("GRANTS_CACHE_TTL_SECONDS", 30), // per-user project grants; grant mutations bust the cache directly
            cache_max_bytes: parse_or("CACHE_MAX_BYTES", 209_715_200),          // 200MB default

            // Application metadata
            deployment: parse_or("DEPLOYMENT", Deployment::Local),

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

            trusted_proxy_cidrs: env::var("TRUSTED_PROXY_CIDRS")
                .ok()
                .filter(|s| !s.is_empty())
                .map_or_else(crate::common::rate_limit::default_trusted_proxies, |v| {
                    v.split(',')
                        .filter_map(crate::common::rate_limit::IpCidr::parse)
                        .collect()
                }),

            // CORS
            cors_allowed_origins: string_or(
                "CORS_ALLOWED_ORIGINS",
                "http://localhost:5173,http://localhost:3005",
            )
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),

            // Connection pool
            db_max_connections: parse_or("DB_MAX_CONNECTIONS", 25),
            db_min_connections: parse_or("DB_MIN_CONNECTIONS", 5),

            // Request timeout
            request_timeout_seconds: parse_or("REQUEST_TIMEOUT_SECONDS", 60),

            default_readings_lookback_days: parse_or("DEFAULT_READINGS_LOOKBACK_DAYS", 7),

            // Public API rate limit, modest by design; responses are cache-backed.
            // Defaults: burst 10, 1 token per 2s ⇒ ~30/min sustained.
            public_rate_limit_burst: parse_or("PUBLIC_RATE_LIMIT_BURST", 10),
            public_rate_limit_period_secs: parse_or("PUBLIC_RATE_LIMIT_PERIOD_SECS", 2),

            auth_rate_limit_per_second: parse_or("AUTH_RATE_LIMIT_PER_SECOND", 100),
            auth_rate_limit_burst: parse_or("AUTH_RATE_LIMIT_BURST", 200),

            audit_api_token_use: parse_or("AUDIT_API_TOKEN_USE", true),

            request_summary_seconds: parse_or("REQUEST_SUMMARY_SECONDS", 30),

            tools_runner_url: env::var("TOOLS_RUNNER_URL")
                .ok()
                .map(|u| u.trim_end_matches('/').to_string())
                .filter(|u| !u.is_empty()),
            tools_runner_timeout_seconds: parse_or("TOOLS_RUNNER_TIMEOUT_SECONDS", 60),

            meteoswiss_base_url: string_or(
                "METEOSWISS_BASE_URL",
                "https://data.geo.admin.ch/ch.meteoschweiz.ogd-smn",
            )
            .trim_end_matches('/')
            .to_string(),
            meteoswiss_latest_url: string_or(
                "METEOSWISS_LATEST_URL",
                "https://data.geo.admin.ch/ch.meteoschweiz.messwerte-aktuell/VQHA80.csv",
            ),
            meteoswiss_interval_seconds: parse_or("METEOSWISS_INTERVAL_SECONDS", 600),
            meteoswiss_recent_interval_seconds: parse_or(
                "METEOSWISS_RECENT_INTERVAL_SECONDS",
                86_400,
            ),
            meteoswiss_timeout_seconds: parse_or("METEOSWISS_TIMEOUT_SECONDS", 60),

            // Derived-parameter janitor
            janitor_interval_seconds: parse_or("JANITOR_INTERVAL_SECONDS", 3600),
            janitor_full_refresh_seconds: parse_or("JANITOR_FULL_REFRESH_SECONDS", 86_400),
            janitor_retention_days: parse_or("JANITOR_RETENTION_DAYS", 180),
            job_maintenance_retention_days: parse_or("JOB_MAINTENANCE_RETENTION_DAYS", 14),
            job_maintenance_max_rows: parse_or("JOB_MAINTENANCE_MAX_ROWS", 50_000),

            alarm_sweep_interval_seconds: parse_or("ALARM_SWEEP_INTERVAL_SECONDS", 300),

            sync_event_sweep_interval_seconds: parse_or("SYNC_EVENT_SWEEP_INTERVAL_SECONDS", 300),
            sync_event_stale_after_seconds: parse_or("SYNC_EVENT_STALE_AFTER_SECONDS", 3600),
            sync_event_retention_days: parse_or("SYNC_EVENT_RETENTION_DAYS", 90),
            ingest_receipt_retention_days: parse_or("INGEST_RECEIPT_RETENTION_DAYS", 365),

            sync_session_token_ttl_secs: parse_or("SYNC_SESSION_TOKEN_TTL_SECS", 900),
            sync_command_expiry_secs: parse_or("SYNC_COMMAND_EXPIRY_SECS", 300),
            sync_health_healthy_secs: parse_or("SYNC_HEALTH_HEALTHY_SECS", 90),
            sync_health_warning_secs: parse_or("SYNC_HEALTH_WARNING_SECS", 300),
            sync_client_id_prefix: string_or("SYNC_CLIENT_ID_PREFIX", "svc_"),

            job_max_retries: parse_or("JOB_MAX_RETRIES", 3),
            job_retry_backoff_seconds: parse_or("JOB_RETRY_BACKOFF_SECONDS", 60),

            notify_poll_interval_seconds: parse_or("NOTIFY_POLL_INTERVAL_SECONDS", 60),
            identity_reconcile_interval_seconds: parse_or(
                "IDENTITY_RECONCILE_INTERVAL_SECONDS",
                300,
            ),
            notify_health_interval_seconds: parse_or("NOTIFY_HEALTH_INTERVAL_SECONDS", 300),
            battery_cutoff_volts: parse_or("BATTERY_CUTOFF_VOLTS", 10.5),
            battery_forecast_alert_days: parse_or("BATTERY_FORECAST_ALERT_DAYS", 14),
            stale_data_threshold_hours: parse_or("STALE_DATA_THRESHOLD_HOURS", 6),
            dashboard_base_url: env::var("DASHBOARD_BASE_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            vapid_private_key_pem: env::var("VAPID_PRIVATE_KEY_PEM")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|s| s.replace("\\n", "\n")),
            vapid_public_key: env::var("VAPID_PUBLIC_KEY").ok().filter(|s| !s.is_empty()),
            vapid_subject: env::var("VAPID_SUBJECT").ok().filter(|s| !s.is_empty()),
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

/// The origins a deployed instance may serve cross-origin, and the ones it refuses to.
///
/// The compiled-in default is the two local dev servers, so an overlay that names no origin leaves
/// a deployed API allowing credentialed requests from anything a browser loads off `localhost`.
/// A loopback origin is meaningful only where the browser and the API share the machine, so
/// outside `local` it is dropped and named rather than served.
#[must_use]
pub fn served_cors_origins(
    deployment: Deployment,
    origins: &[String],
) -> (Vec<String>, Vec<String>) {
    if matches!(deployment, Deployment::Local) {
        return (origins.to_vec(), Vec::new());
    }
    let loopback = |o: &String| {
        let host = o.split("://").nth(1).unwrap_or(o);
        let host = host.split('/').next().unwrap_or(host);
        let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
        matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
    };
    let (dropped, kept): (Vec<String>, Vec<String>) =
        origins.iter().cloned().partition(|o| loopback(o));
    (kept, dropped)
}

#[cfg(test)]
#[path = "tests/config.rs"]
mod tests;
