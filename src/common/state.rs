use chrono::{DateTime, Utc};
use moka::future::Cache;
use sea_orm::DatabaseConnection;
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::vaisala::VaisalaClient;

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
    pub vaisala_client: Arc<VaisalaClient>,
    pub response_cache: ResponseCache,
}

impl AppState {
    pub fn new(db: DatabaseConnection, config: Config, vaisala_client: VaisalaClient) -> Self {
        // Cache weighted by byte size, not entry count
        let cache: ResponseCache = Cache::builder()
            .weigher(|_key: &String, value: &CachedResponse| -> u32 {
                // Weight is the size in bytes (capped at u32::MAX)
                value.data.len().try_into().unwrap_or(u32::MAX)
            })
            .max_capacity(config.cache_max_bytes)
            .time_to_live(Duration::from_secs(config.cache_ttl_seconds))
            .build();

        Self {
            db,
            config: Arc::new(config),
            vaisala_client: Arc::new(vaisala_client),
            response_cache: cache,
        }
    }
}
