use axum_keycloak_auth::instance::KeycloakAuthInstance;
use chrono::{DateTime, Utc};
use moka::future::Cache;
use river_data_core::server::SyncState;
use sea_orm::DatabaseConnection;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{Mutex, broadcast};

/// Cached CSV text for the import staging flow. Keyed by session UUID.
pub type ImportStagingCache = Cache<String, Arc<String>>;

use crate::config::Config;
use crate::routes::private::api_tokens::services::TokenCache;
use crate::routes::private::notifications::authz::Authorizer;
use super::bulk::{BulkSemaphore, new_bulk_semaphore};
use crate::routes::public::services::{PublicConfigCache, new_public_config_cache};

/// Per-token request limiters, keyed by token id. Each entry holds the configured
/// `rate_limit_per_second` (so a changed limit rebuilds the limiter) and a direct governor
/// limiter. Tokens with no configured limit never enter this cache (they are unlimited). Bounded
/// by capacity + idle eviction so it can't grow without limit.
pub type TokenRateLimiters =
    Cache<uuid::Uuid, (i32, Arc<governor::DefaultDirectRateLimiter>)>;

/// Build an empty per-token rate-limiter registry (max 10k live keys, evicted after 1h idle).
#[must_use]
pub fn new_token_rate_limiters() -> TokenRateLimiters {
    Cache::builder()
        .max_capacity(10_000)
        .time_to_idle(Duration::from_secs(3600))
        .build()
}

/// Server-sent event pushed to connected clients via the `/api/events` SSE stream.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AppEvent {
    JobCreated { job_id: uuid::Uuid },
    JobProgress { job_id: uuid::Uuid, status: String, progress: Option<i32>, total: Option<i32> },
    JobCompleted { job_id: uuid::Uuid, status: String, readings_updated: Option<i32>, error_message: Option<String> },
    JobLog { job_id: uuid::Uuid, seq: i64, level: String, message: String, context: serde_json::Value },
    DataIngested { site_id: Option<uuid::Uuid>, parameter_id: Option<uuid::Uuid>, stream_id: uuid::Uuid, count: usize },
    AlarmStateChanged { opened: usize, resolved: usize },
}

pub type EventSender = broadcast::Sender<AppEvent>;

/// Global handle so CrudCrate operation hooks (which only receive `&self` + `db`)
/// can emit events without modifying the CrudCrate trait signatures.
static GLOBAL_EVENT_SENDER: OnceLock<EventSender> = OnceLock::new();

/// Returns a clone of the global event sender, if initialised.
pub fn global_event_sender() -> Option<EventSender> {
    GLOBAL_EVENT_SENDER.get().cloned()
}

/// Global handle to the running `AppState` so worker-run scheduled Jobs (which receive only a
/// `JobContext` carrying `db`/`events`/`params`) can reach config and shared in-process services —
/// most importantly the live `Authorizer` cache the identity-reconcile job re-resolves against.
/// Same single-replica, set-once contract as [`GLOBAL_EVENT_SENDER`]: the first `AppState::new` wins
/// (later ones in tests are ignored), so this is the real serving state in `main.rs`.
static GLOBAL_APP_STATE: OnceLock<AppState> = OnceLock::new();

/// Returns a clone of the global `AppState`, if initialised. `None` in contexts that never built one.
#[must_use]
pub fn global_app_state() -> Option<AppState> {
    GLOBAL_APP_STATE.get().cloned()
}

/// Cached admin token: (access_token, expiry).
type AdminTokenCache = Arc<Mutex<Option<(String, DateTime<Utc>)>>>;

/// Cached Keycloak admin API credentials and token.
#[derive(Clone)]
pub struct KeycloakAdmin {
    pub http_client: reqwest::Client,
    pub client_id: String,
    pub client_secret: String,
    pub token_cache: AdminTokenCache,
}

/// Cached response with metadata for freshness checking
#[derive(Clone)]
pub struct CachedResponse {
    pub data: Arc<Vec<u8>>,
    pub max_time: Option<DateTime<Utc>>,
}

/// Cache for API responses. Key is request params, value is serialized response + metadata.
/// Weighted by byte size to enforce memory limit.
pub type ResponseCache = Cache<String, CachedResponse>;

#[derive(Clone)]
pub struct AppState {
    pub db: DatabaseConnection,
    pub config: Arc<Config>,
    pub response_cache: ResponseCache,
    pub public_config_cache: PublicConfigCache,
    pub keycloak_auth_instance: Option<Arc<KeycloakAuthInstance>>,
    pub bulk_semaphore: BulkSemaphore,
    pub keycloak_admin: Option<KeycloakAdmin>,
    pub token_cache: TokenCache,
    pub token_rate_limiters: TokenRateLimiters,
    pub events: EventSender,
    pub import_staging: ImportStagingCache,
    /// Live role resolver for Telegram chats (anti-backdoor). Shared so the user-management revoke
    /// path can invalidate a sub's cached role immediately.
    pub authorizer: Arc<Authorizer>,
}

impl AppState {
    #[must_use]
    pub fn new(
        db: DatabaseConnection,
        config: Config,
        keycloak_auth_instance: Option<Arc<KeycloakAuthInstance>>,
    ) -> Self {
        // Cache weighted by byte size, not entry count
        let cache: ResponseCache = Cache::builder()
            .weigher(|_key: &String, value: &CachedResponse| -> u32 {
                // Weight is the size in bytes (capped at u32::MAX)
                value.data.len().try_into().unwrap_or(u32::MAX)
            })
            .max_capacity(config.cache_max_bytes)
            .time_to_live(Duration::from_secs(config.cache_ttl_seconds))
            .build();

        let bulk_semaphore = new_bulk_semaphore(config.bulk_concurrent_limit);

        let keycloak_admin = config.keycloak_admin_client_secret.as_ref().map(|secret| {
            let client_id = config
                .keycloak_admin_client_id
                .clone()
                .or_else(|| config.keycloak_client_id.clone())
                .unwrap_or_default();
            tracing::info!(client_id = %client_id, "Keycloak admin proxy enabled");
            KeycloakAdmin {
                http_client: reqwest::Client::new(),
                client_id,
                client_secret: secret.clone(),
                token_cache: Arc::new(Mutex::new(None)),
            }
        });

        let token_cache = crate::routes::private::api_tokens::services::new_token_cache(
            config.token_cache_ttl_seconds,
        );
        let token_rate_limiters = new_token_rate_limiters();
        let (events, _) = broadcast::channel(256);
        let _ = GLOBAL_EVENT_SENDER.set(events.clone());

        let import_staging: ImportStagingCache = Cache::builder()
            .weigher(|_key: &String, value: &Arc<String>| -> u32 {
                value.len().try_into().unwrap_or(u32::MAX)
            })
            .max_capacity(500 * 1024 * 1024) // 500 MB
            .time_to_live(Duration::from_secs(600)) // 10 minutes
            .build();

        let state = Self {
            db,
            config: Arc::new(config),
            response_cache: cache,
            public_config_cache: new_public_config_cache(),
            keycloak_auth_instance,
            bulk_semaphore,
            keycloak_admin,
            token_cache,
            token_rate_limiters,
            events,
            import_staging,
            authorizer: Arc::new(Authorizer::new()),
        };
        // Publish the serving state so worker-run scheduled Jobs can reach config + the shared
        // Authorizer. Set-once: the first construction (the real one in `main.rs`) wins.
        let _ = GLOBAL_APP_STATE.set(state.clone());
        state
    }
}

impl SyncState for AppState {
    fn db(&self) -> &DatabaseConnection {
        &self.db
    }
}
