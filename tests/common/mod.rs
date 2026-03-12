//! Shared test infrastructure for river-data-api e2e tests.
//!
//! Provides database setup, seed data insertion, app builder, and cleanup helpers.
//! Requires a running TimescaleDB instance at `DATABASE_URL`.

use axum::Router;
use chrono::{DateTime, Duration, Utc};
use river_db::common::AppState;
use river_db::config::Config;
use river_db::services::api_token::hash_token;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;

// ============================================================================
// Fixed IDs for deterministic tests
// ============================================================================

/// Project: "Test River Project"
pub const PROJECT_ID: &str = "00000000-0000-4000-a000-000000000001";

/// Site 1: "Upstream Station"
pub const SITE1_ID: &str = "00000000-0000-4000-a000-000000000010";
/// Site 2: "Downstream Station"
pub const SITE2_ID: &str = "00000000-0000-4000-a000-000000000020";

// Global parameter IDs (catalog)
pub const GLOBAL_PARAM_TEMP_ID: &str = "00000000-0000-4000-b000-000000000001";
pub const GLOBAL_PARAM_DO_ID: &str = "00000000-0000-4000-b000-000000000002";
pub const GLOBAL_PARAM_COND_ID: &str = "00000000-0000-4000-b000-000000000003";
pub const GLOBAL_PARAM_TURB_ID: &str = "00000000-0000-4000-b000-000000000004";
pub const GLOBAL_PARAM_DEPTH_ID: &str = "00000000-0000-4000-b000-000000000005";

// Site 1 site_parameter IDs
pub const PARAM_S1_TEMP_ID: &str = "00000000-0000-4000-a000-000000000101";
pub const PARAM_S1_DO_ID: &str = "00000000-0000-4000-a000-000000000102";
pub const PARAM_S1_COND_ID: &str = "00000000-0000-4000-a000-000000000103";
pub const PARAM_S1_TURB_ID: &str = "00000000-0000-4000-a000-000000000104";
pub const PARAM_S1_DEPTH_ID: &str = "00000000-0000-4000-a000-000000000105";

// Site 2 site_parameter IDs
pub const PARAM_S2_TEMP_ID: &str = "00000000-0000-4000-a000-000000000201";
pub const PARAM_S2_DO_ID: &str = "00000000-0000-4000-a000-000000000202";
pub const PARAM_S2_COND_ID: &str = "00000000-0000-4000-a000-000000000203";
pub const PARAM_S2_TURB_ID: &str = "00000000-0000-4000-a000-000000000204";

/// Base time for all readings: 2025-01-15T00:00:00Z
pub fn base_time() -> DateTime<Utc> {
    "2025-01-15T00:00:00Z".parse().unwrap()
}

/// Total number of readings per parameter (48h at 10-min intervals)
pub const READINGS_PER_PARAM: usize = 288;

/// All site_parameter IDs (9 total)
pub fn all_site_parameter_ids() -> Vec<&'static str> {
    vec![
        PARAM_S1_TEMP_ID,
        PARAM_S1_DO_ID,
        PARAM_S1_COND_ID,
        PARAM_S1_TURB_ID,
        PARAM_S1_DEPTH_ID,
        PARAM_S2_TEMP_ID,
        PARAM_S2_DO_ID,
        PARAM_S2_COND_ID,
        PARAM_S2_TURB_ID,
    ]
}

// ============================================================================
// Database setup
// ============================================================================

/// Connect to the test database and run migrations.
///
/// Reads `DATABASE_URL` from the environment (supports `.env` via dotenvy).
/// The database must be a TimescaleDB instance.
pub async fn setup_test_db() -> DatabaseConnection {
    dotenvy::dotenv().ok();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    let db = Database::connect(&url)
        .await
        .expect("Failed to connect to test database");

    // Run migrations (idempotent — safe to call multiple times)
    migration::Migrator::up(&db, None)
        .await
        .expect("Failed to run migrations");

    db
}

// ============================================================================
// App builder
// ============================================================================

/// Build a test-ready Router with the same structure as production,
/// but with rate limiting disabled.
pub fn build_test_app(db: DatabaseConnection) -> Router {
    let config = test_config();
    let state = AppState::new(db, config, None);
    river_db::routes::build_router(state)
}

/// Build a test Config with rate limiting disabled and dummy Vaisala credentials.
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
        cache_ttl_seconds: 0, // disable caching in tests
        cache_max_bytes: 0,
        deployment: river_db::config::Deployment::Local,
        keycloak_url: None,
        keycloak_realm: None,
        keycloak_client_id: None,
        keycloak_admin_client_id: None,
        keycloak_admin_client_secret: None,
    }
}

// ============================================================================
// Cleanup
// ============================================================================

/// Truncate all tables in FK-safe order, including continuous aggregate data.
pub async fn cleanup_test_db(db: &DatabaseConnection) {
    let stmts = [
        // Continuous aggregates (materialized views) — must drop data manually
        "SELECT remove_continuous_aggregate_policy('readings_monthly', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_weekly', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_daily', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_hourly', if_not_exists => true)",
        // Truncate all tables with CASCADE
        "TRUNCATE alarm_thresholds, sync_state, sensor_calibrations, sensor_deployments, \
         readings, status_events, source_mappings, public_exposed_parameters, api_tokens, \
         site_parameters, sensors, derived_parameter_sources, derived_parameter_definitions, parameters, sites, projects CASCADE",
    ];

    for sql in &stmts {
        let _ = db
            .execute(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                sql.to_string(),
            ))
            .await;
    }
}

// ============================================================================
// Seed data
// ============================================================================

/// Insert a full set of deterministic test data.
///
/// Creates:
/// - 1 project ("Test River Project")
/// - 2 sites ("Upstream Station", "Downstream Station")
/// - 5 global parameters (catalog entries)
/// - 9 site_parameters (5 for site 1, 4 for site 2)
/// - Alarm thresholds for each global parameter (global, site_id=NULL)
/// - Sync state for each site_parameter
/// - 48 hours of readings at 10-minute intervals (~288 per parameter, ~2,592 total)
///   with sinusoidal variation + noise, including readings that exceed alarm/warning thresholds
/// - Refreshes all continuous aggregates
pub async fn seed_test_data(db: &DatabaseConnection) {
    seed_project(db).await;
    seed_sites(db).await;
    seed_parameters(db).await;
    seed_site_parameters(db).await;
    seed_alarm_thresholds(db).await;
    seed_sync_state(db).await;
    seed_readings(db).await;
    refresh_continuous_aggregates(db).await;
}

async fn exec(db: &DatabaseConnection, sql: &str) {
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("SQL failed: {e}\nQuery: {sql}"));
}

async fn seed_project(db: &DatabaseConnection) {
    exec(
        db,
        &format!(
            "INSERT INTO projects (id, name, description) VALUES \
             ('{PROJECT_ID}', 'Test River Project', 'E2E test project')"
        ),
    )
    .await;
}

async fn seed_sites(db: &DatabaseConnection) {
    exec(
        db,
        &format!(
            "INSERT INTO sites (id, project_id, name, latitude, longitude, altitude_m) VALUES \
             ('{SITE1_ID}', '{PROJECT_ID}', 'Upstream Station', 51.5074, -0.1278, 15.0), \
             ('{SITE2_ID}', '{PROJECT_ID}', 'Downstream Station', 51.4900, -0.1100, 8.0)"
        ),
    )
    .await;
}

/// Insert global parameter catalog entries.
async fn seed_parameters(db: &DatabaseConnection) {
    exec(
        db,
        &format!(
            "INSERT INTO parameters (id, name, display_name, default_units, category, data_type) VALUES \
             ('{GLOBAL_PARAM_TEMP_ID}', 'DO_Temperature', 'Water Temperature', '°C', 'measurement', 'numeric'), \
             ('{GLOBAL_PARAM_DO_ID}', 'Dissolved_O2', 'Dissolved Oxygen', 'µM', 'measurement', 'numeric'), \
             ('{GLOBAL_PARAM_COND_ID}', 'Conductivity', 'Conductivity', 'µS/cm', 'measurement', 'numeric'), \
             ('{GLOBAL_PARAM_TURB_ID}', 'Turbidity', 'Turbidity', 'NTU', 'measurement', 'numeric'), \
             ('{GLOBAL_PARAM_DEPTH_ID}', 'Depth', 'Water Depth', 'mm', 'measurement', 'numeric')"
        ),
    )
    .await;
}

/// Site-specific parameter configuration for seed data generation.
struct ParamConfig {
    site_param_id: &'static str,
    site_id: &'static str,
    global_param_id: &'static str,
    name: &'static str,
    sensor_type: &'static str,
    display_units: &'static str,
    units_name: &'static str,
    units_min: f64,
    units_max: f64,
    decimal_places: i16,
    /// (mean, amplitude) for sinusoidal value generation
    value_mean: f64,
    value_amplitude: f64,
}

fn param_configs() -> Vec<ParamConfig> {
    vec![
        // Site 1
        ParamConfig {
            site_param_id: PARAM_S1_TEMP_ID,
            site_id: SITE1_ID,
            global_param_id: GLOBAL_PARAM_TEMP_ID,
            name: "DO_Temperature",
            sensor_type: "DO_Temperature",
            display_units: "°C",
            units_name: "Degrees Celsius",
            units_min: -10.0,
            units_max: 50.0,
            decimal_places: 2,
            value_mean: 13.0,
            value_amplitude: 9.0,
        },
        ParamConfig {
            site_param_id: PARAM_S1_DO_ID,
            site_id: SITE1_ID,
            global_param_id: GLOBAL_PARAM_DO_ID,
            name: "Dissolved_O2",
            sensor_type: "Dissolved_O2",
            display_units: "µM",
            units_name: "Micromolar",
            units_min: 0.0,
            units_max: 625.0,
            decimal_places: 1,
            value_mean: 250.0,
            value_amplitude: 100.0,
        },
        ParamConfig {
            site_param_id: PARAM_S1_COND_ID,
            site_id: SITE1_ID,
            global_param_id: GLOBAL_PARAM_COND_ID,
            name: "Conductivity",
            sensor_type: "Conductivity",
            display_units: "µS/cm",
            units_name: "Microsiemens per centimeter",
            units_min: 0.0,
            units_max: 2000.0,
            decimal_places: 1,
            value_mean: 450.0,
            value_amplitude: 350.0,
        },
        ParamConfig {
            site_param_id: PARAM_S1_TURB_ID,
            site_id: SITE1_ID,
            global_param_id: GLOBAL_PARAM_TURB_ID,
            name: "Turbidity",
            sensor_type: "Turbidity",
            display_units: "NTU",
            units_name: "Nephelometric Turbidity Units",
            units_min: 0.0,
            units_max: 1000.0,
            decimal_places: 1,
            value_mean: 50.0,
            value_amplitude: 45.0,
        },
        ParamConfig {
            site_param_id: PARAM_S1_DEPTH_ID,
            site_id: SITE1_ID,
            global_param_id: GLOBAL_PARAM_DEPTH_ID,
            name: "Depth",
            sensor_type: "Depth",
            display_units: "mm",
            units_name: "Millimeters",
            units_min: 0.0,
            units_max: 3000.0,
            decimal_places: 0,
            value_mean: 500.0,
            value_amplitude: 450.0,
        },
        // Site 2
        ParamConfig {
            site_param_id: PARAM_S2_TEMP_ID,
            site_id: SITE2_ID,
            global_param_id: GLOBAL_PARAM_TEMP_ID,
            name: "DO_Temperature",
            sensor_type: "DO_Temperature",
            display_units: "°C",
            units_name: "Degrees Celsius",
            units_min: -10.0,
            units_max: 50.0,
            decimal_places: 2,
            value_mean: 14.0,
            value_amplitude: 8.0,
        },
        ParamConfig {
            site_param_id: PARAM_S2_DO_ID,
            site_id: SITE2_ID,
            global_param_id: GLOBAL_PARAM_DO_ID,
            name: "Dissolved_O2",
            sensor_type: "Dissolved_O2",
            display_units: "µM",
            units_name: "Micromolar",
            units_min: 0.0,
            units_max: 625.0,
            decimal_places: 1,
            value_mean: 230.0,
            value_amplitude: 90.0,
        },
        ParamConfig {
            site_param_id: PARAM_S2_COND_ID,
            site_id: SITE2_ID,
            global_param_id: GLOBAL_PARAM_COND_ID,
            name: "Conductivity",
            sensor_type: "Conductivity",
            display_units: "µS/cm",
            units_name: "Microsiemens per centimeter",
            units_min: 0.0,
            units_max: 2000.0,
            decimal_places: 1,
            value_mean: 500.0,
            value_amplitude: 300.0,
        },
        ParamConfig {
            site_param_id: PARAM_S2_TURB_ID,
            site_id: SITE2_ID,
            global_param_id: GLOBAL_PARAM_TURB_ID,
            name: "Turbidity",
            sensor_type: "Turbidity",
            display_units: "NTU",
            units_name: "Nephelometric Turbidity Units",
            units_min: 0.0,
            units_max: 1000.0,
            decimal_places: 1,
            value_mean: 60.0,
            value_amplitude: 50.0,
        },
    ]
}

/// Insert site_parameters linking sites to global parameters.
async fn seed_site_parameters(db: &DatabaseConnection) {
    let configs = param_configs();
    let mut values = Vec::with_capacity(configs.len());

    for p in &configs {
        values.push(format!(
            "('{id}', '{site_id}', '{param_id}', '{name}', '{sensor_type}', '{units}', '{uname}', {umin}, {umax}, {dp}, 600, true)",
            id = p.site_param_id,
            site_id = p.site_id,
            param_id = p.global_param_id,
            name = p.name,
            sensor_type = p.sensor_type,
            units = p.display_units,
            uname = p.units_name,
            umin = p.units_min,
            umax = p.units_max,
            dp = p.decimal_places,
        ));
    }

    exec(
        db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type, display_units, units_name, units_min, units_max, decimal_places, sample_interval_sec, is_active) VALUES {}",
            values.join(", ")
        ),
    )
    .await;
}

/// Alarm threshold configuration.
struct ThresholdConfig {
    global_param_id: &'static str,
    site_id: Option<&'static str>,
    warning_min: Option<f64>,
    warning_max: Option<f64>,
    alarm_min: Option<f64>,
    alarm_max: Option<f64>,
    description: &'static str,
}

fn threshold_configs() -> Vec<ThresholdConfig> {
    vec![
        ThresholdConfig {
            global_param_id: GLOBAL_PARAM_TEMP_ID,
            site_id: None,
            warning_min: Some(0.5),
            warning_max: Some(20.0),
            alarm_min: Some(0.0),
            alarm_max: Some(25.0),
            description: "Water temperature thresholds",
        },
        ThresholdConfig {
            global_param_id: GLOBAL_PARAM_DO_ID,
            site_id: None,
            warning_min: Some(120.0),
            warning_max: Some(360.0),
            alarm_min: Some(0.0),
            alarm_max: Some(625.0),
            description: "Dissolved oxygen thresholds",
        },
        ThresholdConfig {
            global_param_id: GLOBAL_PARAM_COND_ID,
            site_id: None,
            warning_min: Some(100.0),
            warning_max: Some(900.0),
            alarm_min: Some(0.0),
            alarm_max: Some(1000.0),
            description: "Conductivity thresholds",
        },
        ThresholdConfig {
            global_param_id: GLOBAL_PARAM_TURB_ID,
            site_id: None,
            warning_min: None,
            warning_max: Some(100.0),
            alarm_min: Some(0.0),
            alarm_max: Some(500.0),
            description: "Turbidity thresholds",
        },
        ThresholdConfig {
            global_param_id: GLOBAL_PARAM_DEPTH_ID,
            site_id: None,
            warning_min: Some(100.0),
            warning_max: Some(1000.0),
            alarm_min: Some(0.0),
            alarm_max: Some(2000.0),
            description: "Depth thresholds",
        },
    ]
}

async fn seed_alarm_thresholds(db: &DatabaseConnection) {
    let configs = threshold_configs();
    let mut values = Vec::with_capacity(configs.len());

    for t in &configs {
        let wmin = t.warning_min.map_or("NULL".to_string(), |v| v.to_string());
        let wmax = t.warning_max.map_or("NULL".to_string(), |v| v.to_string());
        let amin = t.alarm_min.map_or("NULL".to_string(), |v| v.to_string());
        let amax = t.alarm_max.map_or("NULL".to_string(), |v| v.to_string());
        let site = t.site_id.map_or("NULL".to_string(), |s| format!("'{s}'"));
        values.push(format!(
            "(gen_random_uuid(), '{pid}', {site}, {wmin}, {wmax}, {amin}, {amax}, '{desc}')",
            pid = t.global_param_id,
            desc = t.description,
        ));
    }

    exec(
        db,
        &format!(
            "INSERT INTO alarm_thresholds (id, parameter_id, site_id, warning_min, warning_max, alarm_min, alarm_max, description) VALUES {}",
            values.join(", ")
        ),
    )
    .await;
}

async fn seed_sync_state(db: &DatabaseConnection) {
    let ids = all_site_parameter_ids();
    let now = Utc::now().to_rfc3339();
    let mut values = Vec::with_capacity(ids.len());

    for id in ids {
        values.push(format!(
            "('{id}', '{now}', '{now}', 'success', NULL, 0, '{now}')"
        ));
    }

    exec(
        db,
        &format!(
            "INSERT INTO sync_state (site_parameter_id, last_data_time, last_sync_attempt, sync_status, error_message, retry_count, last_full_sync) VALUES {}",
            values.join(", ")
        ),
    )
    .await;
}

/// Generate a realistic sensor value at a given time step.
///
/// Uses sinusoidal oscillation (24-hour period) + deterministic noise.
/// At specific indices, injects values that exceed warning/alarm thresholds.
fn generate_value(cfg: &ParamConfig, step: usize) -> f64 {
    let t = step as f64;
    let period = 144.0; // 144 steps = 24 hours (at 10-min intervals)

    let phase = 2.0 * std::f64::consts::PI * t / period;
    let base = cfg.value_mean + cfg.value_amplitude * phase.sin();

    let noise =
        ((t * 7.3 + 11.0).sin() * 3.7 + (t * 13.1).cos() * 2.1) * 0.02 * cfg.value_amplitude;

    let mut value = base + noise;

    match cfg.sensor_type {
        "DO_Temperature" => {
            if step == 50 {
                value = 22.0;
            } else if step == 100 {
                value = 26.0;
            } else if step == 200 {
                value = 0.3;
            }
        }
        "Dissolved_O2" => {
            if step == 50 {
                value = 370.0;
            } else if step == 100 {
                value = 630.0;
            } else if step == 200 {
                value = 110.0;
            }
        }
        "Conductivity" => {
            if step == 50 {
                value = 920.0;
            } else if step == 100 {
                value = 1050.0;
            } else if step == 200 {
                value = 80.0;
            }
        }
        "Turbidity" => {
            if step == 50 {
                value = 150.0;
            } else if step == 100 {
                value = 550.0;
            }
        }
        "Depth" => {
            if step == 50 {
                value = 1100.0;
            } else if step == 100 {
                value = 2100.0;
            } else if step == 200 {
                value = 80.0;
            }
        }
        _ => {}
    }

    value.clamp(cfg.units_min, cfg.units_max)
}

async fn seed_readings(db: &DatabaseConnection) {
    let configs = param_configs();
    let bt = base_time();

    const BATCH_SIZE: usize = 500;
    let mut batch_values: Vec<String> = Vec::with_capacity(BATCH_SIZE);

    for cfg in &configs {
        for step in 0..READINGS_PER_PARAM {
            let time = bt + Duration::minutes((step as i64) * 10);
            let value = generate_value(cfg, step);
            let time_str = time.to_rfc3339();

            batch_values.push(format!(
                "('{site_id}', '{param_id}', '{time_str}', {value})",
                site_id = cfg.site_id,
                param_id = cfg.global_param_id,
            ));

            if batch_values.len() >= BATCH_SIZE {
                flush_readings(db, &batch_values).await;
                batch_values.clear();
            }
        }
    }

    if !batch_values.is_empty() {
        flush_readings(db, &batch_values).await;
    }
}

async fn flush_readings(db: &DatabaseConnection, values: &[String]) {
    let sql = format!(
        "INSERT INTO readings (site_id, parameter_id, time, raw_value) VALUES {} ON CONFLICT DO NOTHING",
        values.join(", ")
    );
    exec(db, &sql).await;
}

async fn refresh_continuous_aggregates(db: &DatabaseConnection) {
    let stmts = [
        "CALL refresh_continuous_aggregate('readings_hourly', '2025-01-14', '2025-01-18')",
        "CALL refresh_continuous_aggregate('readings_daily', '2025-01-14', '2025-01-18')",
        "CALL refresh_continuous_aggregate('readings_weekly', '2025-01-06', '2025-01-20')",
        "CALL refresh_continuous_aggregate('readings_monthly', '2024-12-01', '2025-02-01')",
    ];

    for sql in &stmts {
        exec(db, sql).await;
    }
}

// ============================================================================
// Auth helpers
// ============================================================================

/// Create a JSON permissions object with all permissions enabled.
pub fn full_permissions() -> serde_json::Value {
    serde_json::json!({
        "read_metadata": true,
        "read_data": true,
        "write_metadata": true,
        "write_data": true,
    })
}

/// Seed an API token with given permissions and optional project scope.
/// Returns the raw token string for use in Authorization headers.
pub async fn seed_api_token(
    db: &DatabaseConnection,
    permissions: serde_json::Value,
    project_scope: Option<&str>,
) -> String {
    let raw_token = format!("test-token-{}", uuid::Uuid::new_v4());
    let token_hash = hash_token(&raw_token);
    let scope_sql = project_scope.map_or("NULL".to_string(), |s| format!("'{s}'"));
    let perm_json = serde_json::to_string(&permissions).unwrap();

    exec(
        db,
        &format!(
            "INSERT INTO api_tokens (id, name, token_hash, project_scope, permissions, is_active) \
             VALUES (gen_random_uuid(), 'test-token', '{token_hash}', {scope_sql}, '{perm_json}'::jsonb, true)"
        ),
    )
    .await;

    raw_token
}

/// Seed an API token with a specific expiry time.
pub async fn seed_api_token_with_expiry(
    db: &DatabaseConnection,
    permissions: serde_json::Value,
    project_scope: Option<&str>,
    expires_at: DateTime<Utc>,
) -> String {
    let raw_token = format!("test-token-{}", uuid::Uuid::new_v4());
    let token_hash = hash_token(&raw_token);
    let scope_sql = project_scope.map_or("NULL".to_string(), |s| format!("'{s}'"));
    let perm_json = serde_json::to_string(&permissions).unwrap();
    let expires_str = expires_at.to_rfc3339();

    exec(
        db,
        &format!(
            "INSERT INTO api_tokens (id, name, token_hash, project_scope, permissions, is_active, expires_at) \
             VALUES (gen_random_uuid(), 'test-token', '{token_hash}', {scope_sql}, '{perm_json}'::jsonb, true, '{expires_str}')"
        ),
    )
    .await;

    raw_token
}

/// Seed an inactive API token.
pub async fn seed_inactive_api_token(
    db: &DatabaseConnection,
    permissions: serde_json::Value,
) -> String {
    let raw_token = format!("test-token-{}", uuid::Uuid::new_v4());
    let token_hash = hash_token(&raw_token);
    let perm_json = serde_json::to_string(&permissions).unwrap();

    exec(
        db,
        &format!(
            "INSERT INTO api_tokens (id, name, token_hash, permissions, is_active) \
             VALUES (gen_random_uuid(), 'test-token-inactive', '{token_hash}', '{perm_json}'::jsonb, false)"
        ),
    )
    .await;

    raw_token
}

// ============================================================================
// HTTP test helpers
// ============================================================================

/// Send a GET request (no auth) through the test router and return (status, body_string).
pub async fn get(app: &Router, uri: &str) -> (u16, String) {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let req = axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body).to_string();

    (status, text)
}

/// Send a GET with Bearer token auth.
pub async fn get_with_token(app: &Router, uri: &str, token: &str) -> (u16, String) {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let req = axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body).to_string();

    (status, text)
}

/// Send a GET (no auth) and parse the response body as JSON.
pub async fn get_json(app: &Router, uri: &str) -> (u16, serde_json::Value) {
    let (status, body) = get(app, uri).await;
    let json: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("Failed to parse JSON from {uri}: {e}\nBody: {body}"));
    (status, json)
}

/// Send a GET with Bearer token and parse the response body as JSON.
pub async fn get_json_with_token(
    app: &Router,
    uri: &str,
    token: &str,
) -> (u16, serde_json::Value) {
    let (status, body) = get_with_token(app, uri, token).await;
    let json: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("Failed to parse JSON from {uri}: {e}\nBody: {body}"));
    (status, json)
}

/// Send a POST request with JSON body and Bearer token.
pub async fn post_json_with_token(
    app: &Router,
    uri: &str,
    body: &serde_json::Value,
    token: &str,
) -> (u16, String) {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let req = axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_string(body).unwrap()))
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body).to_string();

    (status, text)
}

/// Send a GET with a custom Authorization header value (for testing malformed auth).
pub async fn get_with_auth_header(app: &Router, uri: &str, auth_value: &str) -> (u16, String) {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let req = axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", auth_value)
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body).to_string();

    (status, text)
}
