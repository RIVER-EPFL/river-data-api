pub mod client;
pub mod compression;
pub mod db;
pub mod e2e;
pub mod fake_portal;
pub mod fixtures;
pub mod jobs;
pub mod keycloak;
pub mod plans;
pub mod profile;
pub mod scratch;
pub mod seed;
pub mod sensor_lifecycle;
pub mod tools_runner;
pub mod tracks;

use river_db::common::{AppState, EventSender};
use river_db::config::Config;
use sea_orm::DatabaseConnection;

// Re-export everything for backwards compatibility with existing tests
pub use client::*;
pub use db::*;
pub use fixtures::*;
pub use seed::*;

/// The workers this process has spawned, so they can be stopped rather than left polling. A
/// handle whose runtime has already gone resolves immediately, so a stale entry costs nothing.
static TEST_WORKERS: std::sync::Mutex<
    Vec<(
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    )>,
> = std::sync::Mutex::new(Vec::new());

/// Stop every worker this process spawned and wait for it to leave the database alone.
///
/// The worker claims whatever is queued, so a test that ends while its own worker is mid-statement
/// leaves that statement running against the next test's cleanup. `cleanup_test_db` calls this
/// first, which makes the fixture the barrier rather than each test's own habit.
pub async fn stop_test_workers() {
    let workers: Vec<_> = TEST_WORKERS
        .lock()
        .map(|mut w| std::mem::take(&mut *w))
        .unwrap_or_default();
    for (shutdown, handle) in workers {
        let _ = shutdown.send(true);
        // A worker that will not stop is holding a job open over the truncate, which is the whole
        // hazard; say so rather than truncating around it. A handle from a runtime that has gone
        // resolves at once.
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(30), handle)
                .await
                .is_ok(),
            "a test worker did not stop; its job is still running against the cleanup"
        );
    }
}

/// Spawn a background job worker for the test, mirroring prod so flipped (`queued`) jobs run to
/// completion under POST-and-poll tests. Stopped by [`stop_test_workers`], which the cleanup runs.
///
/// The registry is the one `main.rs` builds, recurring services included, so a test can enqueue a
/// scheduled service's job (`alarm_sweep`, `janitor_service`, …) and have it run. Nothing here
/// ticks a cadence: no `schedules` row is seeded and no scheduler loop is spawned, so a service
/// runs only when a test asks for it.
///
/// `worker::run` finishes the job it has claimed before it stops, which is what makes it usable as
/// a barrier: when the handle resolves, nothing this worker started is still writing. It finishes
/// that one job and no more, so the wait is bounded by what is in flight rather than by how many
/// jobs a test enqueued; the rows still queued are left for the truncate.
fn spawn_test_worker(state: &AppState) {
    let db = state.db.clone();
    let events = state.events.clone();
    let mut built = river_db::routes::private::reprocessing_jobs::service::build_registry();
    river_db::routes::private::reprocessing_jobs::service::register_scheduled_services(
        &mut built,
        &state.config,
    );
    let registry = std::sync::Arc::new(built);
    let (shutdown, mut stopped) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(async move {
        river_db::routes::private::reprocessing_jobs::service::run_workers(
            db,
            events,
            registry,
            async move {
                while stopped.changed().await.is_ok() {
                    if *stopped.borrow() {
                        return;
                    }
                }
            },
        )
        .await;
    });
    if let Ok(mut workers) = TEST_WORKERS.lock() {
        workers.push((shutdown, handle));
    }
}

/// A seeded application: the database, a router sharing it, and a full-permission API token.
/// Nearly every theme opens on these five calls, so they live here and a test states only what it
/// adds to them.
pub struct Fixture {
    pub db: DatabaseConnection,
    pub app: axum::Router,
    pub token: String,
}

pub async fn seeded_app() -> Fixture {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_test_data(&db).await;
    let token = seed_token_full(&db).await;
    let app = build_test_app(db.clone());
    Fixture { db, app, token }
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
        trusted_proxy_cidrs: river_db::common::rate_limit::default_trusted_proxies(),
        bulk_concurrent_limit: 100,
        cache_ttl_seconds: 0,
        token_cache_ttl_seconds: 1,
        grants_cache_ttl_seconds: 1,
        cache_max_bytes: 0,
        deployment: river_db::config::Deployment::Local,
        meteoswiss_base_url: "http://127.0.0.1:1/ogd-smn".to_string(),
        meteoswiss_interval_seconds: 3600,
        meteoswiss_timeout_seconds: 1,
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
        sync_event_retention_days: 90,
        ingest_receipt_retention_days: 365,
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

/// App + shared state with the response cache on and a byte ceiling small enough to evict. The
/// production default is 200MB, so nothing else in the estate reaches the eviction path.
pub fn build_test_app_with_cache_ceiling(
    db: DatabaseConnection,
    cache_max_bytes: u64,
) -> (axum::Router, AppState) {
    let config = Config {
        cache_max_bytes,
        ..cached_test_config()
    };
    let state = AppState::new(db, config, None);
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

/// The limiter with both tiers set by the caller, for the boundary cases: `test_config` leaves the
/// authenticated tier effectively unlimited and the public one at burst 10 / 2s.
pub fn build_test_app_with_limits(
    db: DatabaseConnection,
    public_burst: u32,
    public_period_secs: u64,
    auth_burst: u32,
) -> axum::Router {
    let config = Config {
        disable_rate_limiting: false,
        public_rate_limit_burst: public_burst,
        public_rate_limit_period_secs: public_period_secs,
        auth_rate_limit_burst: auth_burst,
        auth_rate_limit_per_second: 1,
        ..test_config()
    };
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
