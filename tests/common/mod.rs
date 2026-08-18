pub mod client;
pub mod compression;
pub mod db;
pub mod e2e;
pub mod fixtures;
pub mod keycloak;
pub mod seed;
pub mod sensor_lifecycle;
pub mod tracks;

use river_db::common::{AppState, EventSender};
use river_db::config::Config;
use sea_orm::DatabaseConnection;

// Re-export everything for backwards compatibility with existing tests
pub use client::*;
pub use db::*;
pub use fixtures::*;
pub use seed::*;

/// Spawn a background job worker for the test, mirroring prod so flipped (`queued`) jobs run to
/// completion under POST-and-poll tests. Aborted when the test's runtime drops.
fn spawn_test_worker(state: &AppState) {
    let db = state.db.clone();
    let events = state.events.clone();
    let registry =
        std::sync::Arc::new(river_db::routes::private::reprocessing_jobs::job::build_registry());
    tokio::spawn(async move {
        river_db::routes::private::reprocessing_jobs::worker::run(
            db,
            events,
            registry,
            std::future::pending::<()>(),
        )
        .await;
    });
}

pub fn build_test_app(db: DatabaseConnection) -> axum::Router {
    let config = test_config();
    let state = AppState::new(db, config, None);
    spawn_test_worker(&state);
    river_db::routes::build_router(state)
}

/// Build an `AppState` (and a router that shares it) so tests can both drive HTTP requests and
/// call admin-only handlers (e.g. token revoke/rotate) directly against the same caches.
pub fn build_test_app_with_state(db: DatabaseConnection) -> (axum::Router, AppState) {
    let config = test_config();
    let state = AppState::new(db, config, None);
    spawn_test_worker(&state);
    let app = river_db::routes::build_router(state.clone());
    (app, state)
}

pub fn build_test_app_with_events(db: DatabaseConnection) -> (axum::Router, EventSender) {
    let config = test_config();
    let state = AppState::new(db, config, None);
    spawn_test_worker(&state);
    let events = state.events.clone();
    (river_db::routes::build_router(state), events)
}

pub fn test_config() -> Config {
    Config {
        database_url: std::env::var("DATABASE_URL").unwrap_or_default(),
        api_host: "127.0.0.1".to_string(),
        api_port: 0,
        disable_rate_limiting: true,
        bulk_concurrent_limit: 100,
        cache_ttl_seconds: 0,
        token_cache_ttl_seconds: 1,
        grants_cache_ttl_seconds: 1,
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
        default_readings_lookback_days: 7,
        public_rate_limit_burst: 10,
        public_rate_limit_period_secs: 2,
        // High enough that the authenticated-tier IP limiter never trips in tests (the public-tier
        // limiter is what rate_limit_test exercises); most tests also disable rate limiting entirely.
        auth_rate_limit_per_second: 100_000,
        auth_rate_limit_burst: 100_000,
        // The runner container from the compose test profile; unreachable hosts answer 503.
        tools_runner_url: std::env::var("TOOLS_RUNNER_URL")
            .ok()
            .or_else(|| Some("http://localhost:8006/ocpu".to_string())),
        tools_runner_timeout_seconds: 60,
        // Off by default in tests (keeps token-authed test requests from writing audit rows); the
        // dedicated audit test flips it on against a fresh AppState.
        audit_api_token_use: false,
        request_summary_seconds: 0,
        janitor_interval_seconds: 3600,
        janitor_full_refresh_seconds: 86_400,
        janitor_retention_days: 180,
        job_maintenance_retention_days: 14,
        job_maintenance_max_rows: 50_000,
        alarm_sweep_interval_seconds: 60,
        sync_event_sweep_interval_seconds: 300,
        // Production defaults on purpose: the health test seeds heartbeats at 30s/200s/600s
        // against these thresholds, and a shortened TTL here would hide a real regression.
        sync_session_token_ttl_secs: 900,
        sync_command_expiry_secs: 300,
        sync_health_healthy_secs: 90,
        sync_health_warning_secs: 300,
        sync_client_id_prefix: "svc_".to_string(),
        sync_event_stale_after_seconds: 3600,
        job_max_retries: 3,
        job_retry_backoff_seconds: 60,
        telegram_bot_token: None,
        telegram_bot_username: None,
        enable_telegram_bot: true,
        email_backend: river_db::config::EmailBackend::Disabled,
        smtp_host: None,
        smtp_port: 587,
        smtp_username: None,
        smtp_password: None,
        smtp_from: None,
        graph_tenant_id: None,
        graph_client_id: None,
        graph_client_secret: None,
        graph_sender: None,
        alert_email_to: None,
        notify_poll_interval_seconds: 60,
        identity_reconcile_interval_seconds: 300,
        notify_health_interval_seconds: 300,
        battery_cutoff_volts: 10.5,
        battery_forecast_alert_days: 14,
        stale_data_threshold_hours: 6,
        telegram_grab_flag_for_review: false,
        telegram_alarm_plots: false,
        telegram_alarm_plot_hours: 3,
        telegram_link_idle_days: 30,
        telegram_link_warn_days: 7,
        telegram_link_purge_days: 90,
        telegram_audit_retention_days: 365,
        telegram_link_attest_days: 90,
        telegram_link_attest_warn_days: 14,
        dashboard_base_url: None,
    }
}

/// `test_config()` with the response cache on (300s TTL, 10MB ceiling), the production shape.
pub fn cached_test_config() -> Config {
    Config {
        cache_ttl_seconds: 300,
        cache_max_bytes: 10_000_000,
        ..test_config()
    }
}

/// App with the response cache enabled, for tests that assert on cache hits and cache keys.
pub fn build_test_app_with_cache(db: DatabaseConnection) -> axum::Router {
    let state = AppState::new(db, cached_test_config(), None);
    spawn_test_worker(&state);
    river_db::routes::build_router(state)
}

/// App + shared state with the response cache enabled, so a test can also reach the cache directly.
pub fn build_test_app_with_cache_and_state(db: DatabaseConnection) -> (axum::Router, AppState) {
    let state = AppState::new(db, cached_test_config(), None);
    spawn_test_worker(&state);
    let app = river_db::routes::build_router(state.clone());
    (app, state)
}

pub fn build_test_app_with_rate_limiting(db: DatabaseConnection) -> axum::Router {
    let mut config = test_config();
    config.disable_rate_limiting = false;
    let state = AppState::new(db, config, None);
    spawn_test_worker(&state);
    river_db::routes::build_router(state)
}

/// App + shared state with API-token-use auditing enabled (off by default in tests).
pub fn build_test_app_with_audit(db: DatabaseConnection) -> (axum::Router, AppState) {
    let mut config = test_config();
    config.audit_api_token_use = true;
    let state = AppState::new(db, config, None);
    spawn_test_worker(&state);
    let app = river_db::routes::build_router(state.clone());
    (app, state)
}
