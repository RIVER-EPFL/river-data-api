//! Shared test infrastructure for river-data-api e2e tests.
//!
//! Provides database setup, seed data insertion, app builder, and cleanup helpers.
//! Requires a running TimescaleDB instance at `DATABASE_URL`.

use axum::Router;
use chrono::{DateTime, Duration, Utc};
use river_db::common::AppState;
use river_db::config::Config;
use river_db::vaisala::VaisalaClient;
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

// Site 1 parameters
pub const PARAM_S1_TEMP_ID: &str = "00000000-0000-4000-a000-000000000101";
pub const PARAM_S1_DO_ID: &str = "00000000-0000-4000-a000-000000000102";
pub const PARAM_S1_COND_ID: &str = "00000000-0000-4000-a000-000000000103";
pub const PARAM_S1_TURB_ID: &str = "00000000-0000-4000-a000-000000000104";
pub const PARAM_S1_DEPTH_ID: &str = "00000000-0000-4000-a000-000000000105";

// Site 2 parameters
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

/// All parameter IDs (9 total)
pub fn all_parameter_ids() -> Vec<&'static str> {
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
    let vaisala_client = VaisalaClient::new(&config);
    let state = AppState::new(db, config, vaisala_client);
    river_db::routes::build_router(state)
}

/// Build a test Config with rate limiting disabled and dummy Vaisala credentials.
fn test_config() -> Config {
    Config {
        database_url: std::env::var("DATABASE_URL").unwrap_or_default(),
        vaisala_base_url: "http://localhost:9999".to_string(),
        vaisala_bearer_token: "test-token".to_string(),
        vaisala_skip_tls_verify: true,
        vaisala_max_history_days: 90,
        sync_readings_interval_seconds: 9999,
        sync_device_status_interval_seconds: 9999,
        sync_retry_max: 0,
        sync_retry_delay_seconds: 60,
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
    }
}

// ============================================================================
// Cleanup
// ============================================================================

/// Truncate all tables in FK-safe order, including continuous aggregate data.
pub async fn cleanup_test_db(db: &DatabaseConnection) {
    // Truncate in reverse-dependency order. CASCADE handles FKs,
    // but we order explicitly for clarity.
    let stmts = [
        // Continuous aggregates (materialized views) — must drop data manually
        "SELECT remove_continuous_aggregate_policy('readings_monthly', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_weekly', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_daily', if_not_exists => true)",
        "SELECT remove_continuous_aggregate_policy('readings_hourly', if_not_exists => true)",
        // Truncate regular tables with CASCADE
        "TRUNCATE alarm_thresholds, sync_state, calibrations, device_status, readings, source_mappings, parameters, sites, projects CASCADE",
    ];

    for sql in &stmts {
        // Ignore errors on policy removal (may not exist)
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
/// - 9 parameters (5 for site 1, 4 for site 2) with realistic sensor configs
/// - Alarm thresholds for each parameter
/// - Sync state for each parameter
/// - 48 hours of readings at 10-minute intervals (~288 per parameter, ~2,592 total)
///   with sinusoidal variation + noise, including readings that exceed alarm/warning thresholds
/// - Refreshes all continuous aggregates
pub async fn seed_test_data(db: &DatabaseConnection) {
    seed_project(db).await;
    seed_sites(db).await;
    seed_parameters(db).await;
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
            "INSERT INTO projects (id, name, description) VALUES ('{PROJECT_ID}', 'Test River Project', 'E2E test project')"
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

/// Parameter configuration for seed data generation.
struct ParamConfig {
    id: &'static str,
    site_id: &'static str,
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
            id: PARAM_S1_TEMP_ID,
            site_id: SITE1_ID,
            name: "DO_Temperature",
            sensor_type: "DO_Temperature",
            display_units: "°C",
            units_name: "Degrees Celsius",
            units_min: -10.0,
            units_max: 50.0,
            decimal_places: 2,
            value_mean: 13.0,
            value_amplitude: 9.0, // range ~4-22°C
        },
        ParamConfig {
            id: PARAM_S1_DO_ID,
            site_id: SITE1_ID,
            name: "Dissolved_O2",
            sensor_type: "Dissolved_O2",
            display_units: "µM",
            units_name: "Micromolar",
            units_min: 0.0,
            units_max: 625.0,
            decimal_places: 1,
            value_mean: 250.0,
            value_amplitude: 100.0, // range ~150-350 µM
        },
        ParamConfig {
            id: PARAM_S1_COND_ID,
            site_id: SITE1_ID,
            name: "Conductivity",
            sensor_type: "Conductivity",
            display_units: "µS/cm",
            units_name: "Microsiemens per centimeter",
            units_min: 0.0,
            units_max: 2000.0,
            decimal_places: 1,
            value_mean: 450.0,
            value_amplitude: 350.0, // range ~100-800 µS/cm
        },
        ParamConfig {
            id: PARAM_S1_TURB_ID,
            site_id: SITE1_ID,
            name: "Turbidity",
            sensor_type: "Turbidity",
            display_units: "NTU",
            units_name: "Nephelometric Turbidity Units",
            units_min: 0.0,
            units_max: 1000.0,
            decimal_places: 1,
            value_mean: 50.0,
            value_amplitude: 45.0, // range ~5-95, with spikes exceeding thresholds
        },
        ParamConfig {
            id: PARAM_S1_DEPTH_ID,
            site_id: SITE1_ID,
            name: "Depth",
            sensor_type: "Depth",
            display_units: "mm",
            units_name: "Millimeters",
            units_min: 0.0,
            units_max: 3000.0,
            decimal_places: 0,
            value_mean: 500.0,
            value_amplitude: 450.0, // range ~50-950 mm, with spikes
        },
        // Site 2
        ParamConfig {
            id: PARAM_S2_TEMP_ID,
            site_id: SITE2_ID,
            name: "DO_Temperature",
            sensor_type: "DO_Temperature",
            display_units: "°C",
            units_name: "Degrees Celsius",
            units_min: -10.0,
            units_max: 50.0,
            decimal_places: 2,
            value_mean: 14.0,
            value_amplitude: 8.0, // slightly warmer downstream
        },
        ParamConfig {
            id: PARAM_S2_DO_ID,
            site_id: SITE2_ID,
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
            id: PARAM_S2_COND_ID,
            site_id: SITE2_ID,
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
            id: PARAM_S2_TURB_ID,
            site_id: SITE2_ID,
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

async fn seed_parameters(db: &DatabaseConnection) {
    let configs = param_configs();
    let mut values = Vec::with_capacity(configs.len());

    for p in &configs {
        values.push(format!(
            "('{id}', '{site_id}', '{name}', '{sensor_type}', '{units}', '{uname}', {umin}, {umax}, {dp}, 600, true)",
            id = p.id,
            site_id = p.site_id,
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
            "INSERT INTO parameters (id, site_id, name, sensor_type, display_units, units_name, units_min, units_max, decimal_places, sample_interval_sec, is_active) VALUES {}",
            values.join(", ")
        ),
    )
    .await;
}

/// Alarm threshold configuration.
struct ThresholdConfig {
    param_id: &'static str,
    warning_min: Option<f64>,
    warning_max: Option<f64>,
    alarm_min: Option<f64>,
    alarm_max: Option<f64>,
    description: &'static str,
}

fn threshold_configs() -> Vec<ThresholdConfig> {
    vec![
        // Site 1
        ThresholdConfig {
            param_id: PARAM_S1_TEMP_ID,
            warning_min: Some(0.5),
            warning_max: Some(20.0),
            alarm_min: Some(0.0),
            alarm_max: Some(25.0),
            description: "Water temperature thresholds (site 1)",
        },
        ThresholdConfig {
            param_id: PARAM_S1_DO_ID,
            warning_min: Some(120.0),
            warning_max: Some(360.0),
            alarm_min: Some(0.0),
            alarm_max: Some(625.0),
            description: "Dissolved oxygen thresholds (site 1)",
        },
        ThresholdConfig {
            param_id: PARAM_S1_COND_ID,
            warning_min: Some(100.0),
            warning_max: Some(900.0),
            alarm_min: Some(0.0),
            alarm_max: Some(1000.0),
            description: "Conductivity thresholds (site 1)",
        },
        ThresholdConfig {
            param_id: PARAM_S1_TURB_ID,
            warning_min: None,
            warning_max: Some(100.0),
            alarm_min: Some(0.0),
            alarm_max: Some(500.0),
            description: "Turbidity thresholds (site 1)",
        },
        ThresholdConfig {
            param_id: PARAM_S1_DEPTH_ID,
            warning_min: Some(100.0),
            warning_max: Some(1000.0),
            alarm_min: Some(0.0),
            alarm_max: Some(2000.0),
            description: "Depth thresholds (site 1)",
        },
        // Site 2
        ThresholdConfig {
            param_id: PARAM_S2_TEMP_ID,
            warning_min: Some(0.5),
            warning_max: Some(20.0),
            alarm_min: Some(0.0),
            alarm_max: Some(25.0),
            description: "Water temperature thresholds (site 2)",
        },
        ThresholdConfig {
            param_id: PARAM_S2_DO_ID,
            warning_min: Some(120.0),
            warning_max: Some(360.0),
            alarm_min: Some(0.0),
            alarm_max: Some(625.0),
            description: "Dissolved oxygen thresholds (site 2)",
        },
        ThresholdConfig {
            param_id: PARAM_S2_COND_ID,
            warning_min: Some(100.0),
            warning_max: Some(900.0),
            alarm_min: Some(0.0),
            alarm_max: Some(1000.0),
            description: "Conductivity thresholds (site 2)",
        },
        ThresholdConfig {
            param_id: PARAM_S2_TURB_ID,
            warning_min: None,
            warning_max: Some(100.0),
            alarm_min: Some(0.0),
            alarm_max: Some(500.0),
            description: "Turbidity thresholds (site 2)",
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
        values.push(format!(
            "(gen_random_uuid(), '{pid}', {wmin}, {wmax}, {amin}, {amax}, '{desc}')",
            pid = t.param_id,
            desc = t.description,
        ));
    }

    exec(
        db,
        &format!(
            "INSERT INTO alarm_thresholds (id, parameter_id, warning_min, warning_max, alarm_min, alarm_max, description) VALUES {}",
            values.join(", ")
        ),
    )
    .await;
}

async fn seed_sync_state(db: &DatabaseConnection) {
    let ids = all_parameter_ids();
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
            "INSERT INTO sync_state (parameter_id, last_data_time, last_sync_attempt, sync_status, error_message, retry_count, last_full_sync) VALUES {}",
            values.join(", ")
        ),
    )
    .await;
}

/// Generate a realistic sensor value at a given time step.
///
/// Uses sinusoidal oscillation (24-hour period) + deterministic noise.
/// At specific indices, injects values that exceed warning/alarm thresholds
/// to ensure the alarm endpoint has data to return.
fn generate_value(cfg: &ParamConfig, step: usize) -> f64 {
    let t = step as f64;
    let period = 144.0; // 144 steps = 24 hours (at 10-min intervals)

    // Base sinusoidal (diurnal cycle)
    let phase = 2.0 * std::f64::consts::PI * t / period;
    let base = cfg.value_mean + cfg.value_amplitude * phase.sin();

    // Deterministic pseudo-noise: small variation from step index
    let noise =
        ((t * 7.3 + 11.0).sin() * 3.7 + (t * 13.1).cos() * 2.1) * 0.02 * cfg.value_amplitude;

    let mut value = base + noise;

    // Inject threshold-exceeding values at specific steps:
    // Step 50: exceed WARNING max (but not alarm)
    // Step 100: exceed ALARM max
    // Step 200: exceed WARNING min (but not alarm), for parameters that have warning_min
    match cfg.sensor_type {
        "DO_Temperature" => {
            if step == 50 {
                value = 22.0; // > warning_max 20, < alarm_max 25
            } else if step == 100 {
                value = 26.0; // > alarm_max 25
            } else if step == 200 {
                value = 0.3; // < warning_min 0.5, > alarm_min 0
            }
        }
        "Dissolved_O2" => {
            if step == 50 {
                value = 370.0; // > warning_max 360, < alarm_max 625
            } else if step == 100 {
                value = 630.0; // > alarm_max 625
            } else if step == 200 {
                value = 110.0; // < warning_min 120, > alarm_min 0
            }
        }
        "Conductivity" => {
            if step == 50 {
                value = 920.0; // > warning_max 900, < alarm_max 1000
            } else if step == 100 {
                value = 1050.0; // > alarm_max 1000
            } else if step == 200 {
                value = 80.0; // < warning_min 100, > alarm_min 0
            }
        }
        "Turbidity" => {
            if step == 50 {
                value = 150.0; // > warning_max 100, < alarm_max 500
            } else if step == 100 {
                value = 550.0; // > alarm_max 500
            }
            // No warning_min for turbidity
        }
        "Depth" => {
            if step == 50 {
                value = 1100.0; // > warning_max 1000, < alarm_max 2000
            } else if step == 100 {
                value = 2100.0; // > alarm_max 2000
            } else if step == 200 {
                value = 80.0; // < warning_min 100, > alarm_min 0
            }
        }
        _ => {}
    }

    // Clamp to sensor physical range
    value.clamp(cfg.units_min, cfg.units_max)
}

async fn seed_readings(db: &DatabaseConnection) {
    let configs = param_configs();
    let bt = base_time();

    // Build a large batch INSERT for all readings across all parameters.
    // ~2,592 rows total — batch in groups of ~500 for manageable SQL.
    const BATCH_SIZE: usize = 500;
    let mut batch_values: Vec<String> = Vec::with_capacity(BATCH_SIZE);

    for cfg in &configs {
        for step in 0..READINGS_PER_PARAM {
            let time = bt + Duration::minutes((step as i64) * 10);
            let value = generate_value(cfg, step);
            let time_str = time.to_rfc3339();

            batch_values.push(format!(
                "('{time_str}', '{pid}', {value}, true)",
                pid = cfg.id,
            ));

            if batch_values.len() >= BATCH_SIZE {
                flush_readings(db, &batch_values).await;
                batch_values.clear();
            }
        }
    }

    // Flush remaining
    if !batch_values.is_empty() {
        flush_readings(db, &batch_values).await;
    }
}

async fn flush_readings(db: &DatabaseConnection, values: &[String]) {
    let sql = format!(
        "INSERT INTO readings (time, parameter_id, value, logged) VALUES {} ON CONFLICT DO NOTHING",
        values.join(", ")
    );
    exec(db, &sql).await;
}

async fn refresh_continuous_aggregates(db: &DatabaseConnection) {
    let stmts = [
        "CALL refresh_continuous_aggregate('readings_hourly', '2025-01-14', '2025-01-18')",
        "CALL refresh_continuous_aggregate('readings_daily', '2025-01-14', '2025-01-18')",
        // Weekly bucket needs window >= 2 weeks
        "CALL refresh_continuous_aggregate('readings_weekly', '2025-01-06', '2025-01-20')",
        // Monthly bucket needs window >= 2 months
        "CALL refresh_continuous_aggregate('readings_monthly', '2024-12-01', '2025-02-01')",
    ];

    for sql in &stmts {
        exec(db, sql).await;
    }
}

// ============================================================================
// HTTP test helpers
// ============================================================================

/// Send a GET request through the test router and return (status, body_string).
///
/// Uses `tower::ServiceExt::oneshot()` so no actual TCP binding is needed.
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

/// Send a GET and parse the response body as JSON.
pub async fn get_json(app: &Router, uri: &str) -> (u16, serde_json::Value) {
    let (status, body) = get(app, uri).await;
    let json: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("Failed to parse JSON from {uri}: {e}\nBody: {body}"));
    (status, json)
}
