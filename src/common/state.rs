use axum_keycloak_auth::instance::KeycloakAuthInstance;
use chrono::{DateTime, Utc};
use moka::future::Cache;
use sea_orm::DatabaseConnection;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::services::bulk::{BulkSemaphore, new_bulk_semaphore};
use crate::services::public_api_config::{PublicConfigCache, new_public_config_cache};

/// Cached Keycloak admin API credentials and token.
#[derive(Clone)]
pub struct KeycloakAdmin {
    pub http_client: reqwest::Client,
    pub client_id: String,
    pub client_secret: String,
    pub token_cache: Arc<Mutex<Option<(String, DateTime<Utc>)>>>,
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

        Self {
            db,
            config: Arc::new(config),
            response_cache: cache,
            public_config_cache: new_public_config_cache(),
            keycloak_auth_instance,
            bulk_semaphore,
            keycloak_admin,
        }
    }
}
