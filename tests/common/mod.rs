pub mod client;
pub mod db;
pub mod fixtures;
pub mod seed;
pub mod sensor_lifecycle;

use river_db::common::AppState;
use river_db::config::Config;
use sea_orm::DatabaseConnection;

// Re-export everything for backwards compatibility with existing tests
pub use client::*;
pub use db::*;
pub use fixtures::*;
pub use seed::*;

pub fn build_test_app(db: DatabaseConnection) -> axum::Router {
    let config = test_config();
    let state = AppState::new(db, config, None);
    river_db::routes::build_router(state)
}

fn test_config() -> Config {
    Config {
        database_url: std::env::var("DATABASE_URL").unwrap_or_default(),
        api_host: "127.0.0.1".to_string(),
        api_port: 0,
        disable_rate_limiting: true,
        rate_limit_metadata_per_second: 1000,
        rate_limit_metadata_burst: 1000,
        rate_limit_data_per_second: 1000,
        rate_limit_data_burst: 1000,
        bulk_concurrent_limit: 100,
        cache_ttl_seconds: 0,
        cache_max_bytes: 0,
        deployment: river_db::config::Deployment::Local,
        keycloak_url: None,
        keycloak_realm: None,
        keycloak_client_id: None,
        keycloak_admin_client_id: None,
        keycloak_admin_client_secret: None,
        cors_allowed_origins: vec!["*".to_string()],
        db_max_connections: 25,
        db_min_connections: 1,
        request_timeout_seconds: 60,
        max_readings_time_range_days: 90,
        max_aggregates_time_range_days: 365,
        public_max_readings_time_range_days: 30,
        public_max_aggregates_time_range_days: 180,
        default_readings_lookback_days: 7,
    }
}

pub fn build_test_app_with_cache(db: DatabaseConnection) -> axum::Router {
    let mut config = test_config();
    config.cache_ttl_seconds = 300;
    config.cache_max_bytes = 10_000_000;
    let state = AppState::new(db, config, None);
    river_db::routes::build_router(state)
}

pub fn build_test_app_with_rate_limiting(db: DatabaseConnection) -> axum::Router {
    let mut config = test_config();
    config.disable_rate_limiting = false;
    config.rate_limit_metadata_per_second = 2;
    config.rate_limit_metadata_burst = 3;
    config.rate_limit_data_per_second = 2;
    config.rate_limit_data_burst = 3;
    let state = AppState::new(db, config, None);
    river_db::routes::build_router(state)
}
