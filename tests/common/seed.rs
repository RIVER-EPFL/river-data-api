use chrono::{Duration, Utc};
use sea_orm::DatabaseConnection;
use uuid::Uuid;

use super::db::exec;
use super::fixtures::*;
use river_db::routes::private::api_tokens::service::{hash_token, mint_api_token};

// ============================================================================
// Full seed orchestrator
// ============================================================================

pub async fn seed_test_data(db: &DatabaseConnection) {
    seed_project(db).await;
    seed_sites(db).await;
    seed_parameters(db).await;
    seed_site_parameters(db).await;
    seed_alarm_thresholds(db).await;
    seed_streams_for_site_params(db).await;
    seed_readings(db).await;
    refresh_continuous_aggregates(db).await;
}

// ============================================================================
// Individual seeders
// ============================================================================

async fn seed_project(db: &DatabaseConnection) {
    exec(
        db,
        &format!(
            "INSERT INTO projects (id, name, description, data_source) VALUES \
             ('{PROJECT_ID}', 'Test River Project', 'E2E test project', 'test')"
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

async fn seed_parameters(db: &DatabaseConnection) {
    exec(
        db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category) VALUES \
             ('{GLOBAL_PARAM_TEMP_ID}', 'DO_Temperature', 'Water Temperature', '°C', 'measurement'), \
             ('{GLOBAL_PARAM_DO_ID}', 'Dissolved_O2', 'Dissolved Oxygen', 'µM', 'measurement'), \
             ('{GLOBAL_PARAM_COND_ID}', 'Conductivity', 'Conductivity', 'µS/cm', 'measurement'), \
             ('{GLOBAL_PARAM_TURB_ID}', 'Turbidity', 'Turbidity', 'NTU', 'measurement'), \
             ('{GLOBAL_PARAM_DEPTH_ID}', 'Depth', 'Water Depth', 'mm', 'measurement')"
        ),
    )
    .await;
}

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

fn generate_value(cfg: &ParamConfig, step: usize) -> f64 {
    let t = step as f64;
    let period = 144.0;

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

async fn seed_streams_for_site_params(db: &DatabaseConnection) {
    let configs = param_configs();
    let mut values = Vec::with_capacity(configs.len());

    for (i, p) in configs.iter().enumerate() {
        let stream_uuid = format!("00000000-0000-4000-d000-{:012}", i + 1);
        values.push(format!(
            "('{stream_uuid}', 'test-seed', 'seed-{sp_id}', 'Seed {name}', '{sp_id}', NOW(), true)",
            name = p.name,
            sp_id = p.site_param_id,
        ));
    }

    exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, site_parameter_id, paired_at, is_active) VALUES {}",
            values.join(", ")
        ),
    )
    .await;
}

fn stream_id_for_param(cfg_index: usize) -> String {
    format!("00000000-0000-4000-d000-{:012}", cfg_index + 1)
}

async fn seed_readings(db: &DatabaseConnection) {
    let configs = param_configs();
    let bt = base_time();

    const BATCH_SIZE: usize = 500;
    let mut batch_values: Vec<String> = Vec::with_capacity(BATCH_SIZE);

    for (cfg_idx, cfg) in configs.iter().enumerate() {
        let stream = stream_id_for_param(cfg_idx);
        for step in 0..READINGS_PER_PARAM {
            let time = bt + Duration::minutes((step as i64) * 10);
            let value = generate_value(cfg, step);
            let time_str = time.to_rfc3339();

            batch_values.push(format!(
                "('{stream}', '{site_id}', '{param_id}', '{time_str}', {value})",
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
        "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value) VALUES {} ON CONFLICT DO NOTHING",
        values.join(", ")
    );
    exec(db, &sql).await;
}

pub async fn refresh_continuous_aggregates(db: &DatabaseConnection) {
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
// Auth / token helpers
// ============================================================================

pub fn full_permissions() -> serde_json::Value {
    serde_json::json!({
        "read_metadata": true,
        "read_data": true,
        "write_metadata": true,
        "write_data": true,
    })
}

pub fn no_permissions() -> serde_json::Value {
    serde_json::json!({
        "read_metadata": false,
        "read_data": false,
        "write_metadata": false,
        "write_data": false,
    })
}

pub fn perms(read_metadata: bool, read_data: bool, write_metadata: bool, write_data: bool) -> serde_json::Value {
    serde_json::json!({
        "read_metadata": read_metadata,
        "read_data": read_data,
        "write_metadata": write_metadata,
        "write_data": write_data,
    })
}

pub async fn seed_token_full(db: &DatabaseConnection) -> String {
    seed_api_token(db, full_permissions(), None).await
}

pub async fn seed_token_read_metadata_only(db: &DatabaseConnection) -> String {
    seed_api_token(db, perms(true, false, false, false), None).await
}

pub async fn seed_token_read_data_only(db: &DatabaseConnection) -> String {
    seed_api_token(db, perms(false, true, false, false), None).await
}

pub async fn seed_token_write_metadata_only(db: &DatabaseConnection) -> String {
    seed_api_token(db, perms(false, false, true, false), None).await
}

pub async fn seed_token_write_data_only(db: &DatabaseConnection) -> String {
    seed_api_token(db, perms(false, false, false, true), None).await
}

/// Seed a sync service session token with FULL permissions. Mirrors the production
/// auth path where a sync service post-enrollment has unrestricted scope.
/// Returns (raw_token, service_id).
pub async fn seed_sync_session_token(db: &DatabaseConnection) -> (String, Uuid) {
    use chrono::Duration;
    let service_id = Uuid::new_v4();
    let raw_token = format!("sync-session-{}", Uuid::new_v4());
    let token_hash = hash_token(&raw_token);
    let expires_at = (Utc::now() + Duration::hours(1)).to_rfc3339();

    exec(
        db,
        &format!(
            "INSERT INTO sync_services (id, service_type, instance_id, status, created_at, updated_at) \
             VALUES ('{service_id}', 'test', 'test-instance', 'registered', now(), now())"
        ),
    )
    .await;
    exec(
        db,
        &format!(
            "INSERT INTO sync_service_tokens (id, service_id, token_hash, expires_at, created_at) \
             VALUES (gen_random_uuid(), '{service_id}', '{token_hash}', '{expires_at}', now())"
        ),
    )
    .await;

    (raw_token, service_id)
}

pub async fn seed_api_token(
    db: &DatabaseConnection,
    permissions: serde_json::Value,
    project_scope: Option<&str>,
) -> String {
    let minted = mint_api_token();
    let scope_sql = project_scope.map_or("NULL".to_string(), |s| format!("'{s}'"));
    let perm_json = serde_json::to_string(&permissions).unwrap();

    exec(
        db,
        &format!(
            "INSERT INTO api_tokens (id, name, token_hash, token_prefix, project_scope, permissions, is_active) \
             VALUES (gen_random_uuid(), 'test-token', '{}', '{}', {scope_sql}, '{perm_json}'::jsonb, true)",
            minted.token_hash, minted.token_prefix
        ),
    )
    .await;

    minted.raw_token
}

/// Seed an active token carrying a per-token rate limit (requests/second).
pub async fn seed_api_token_with_rate_limit(
    db: &DatabaseConnection,
    permissions: serde_json::Value,
    project_scope: Option<&str>,
    rate_limit_per_second: i32,
) -> String {
    let minted = mint_api_token();
    let scope_sql = project_scope.map_or("NULL".to_string(), |s| format!("'{s}'"));
    let perm_json = serde_json::to_string(&permissions).unwrap();

    exec(
        db,
        &format!(
            "INSERT INTO api_tokens (id, name, token_hash, token_prefix, project_scope, permissions, is_active, rate_limit_per_second) \
             VALUES (gen_random_uuid(), 'test-token-rl', '{}', '{}', {scope_sql}, '{perm_json}'::jsonb, true, {rate_limit_per_second})",
            minted.token_hash, minted.token_prefix
        ),
    )
    .await;

    minted.raw_token
}

pub async fn seed_api_token_with_expiry(
    db: &DatabaseConnection,
    permissions: serde_json::Value,
    project_scope: Option<&str>,
    expires_at: chrono::DateTime<Utc>,
) -> String {
    let minted = mint_api_token();
    let scope_sql = project_scope.map_or("NULL".to_string(), |s| format!("'{s}'"));
    let perm_json = serde_json::to_string(&permissions).unwrap();
    let expires_str = expires_at.to_rfc3339();

    exec(
        db,
        &format!(
            "INSERT INTO api_tokens (id, name, token_hash, token_prefix, project_scope, permissions, is_active, expires_at) \
             VALUES (gen_random_uuid(), 'test-token', '{}', '{}', {scope_sql}, '{perm_json}'::jsonb, true, '{expires_str}')",
            minted.token_hash, minted.token_prefix
        ),
    )
    .await;

    minted.raw_token
}

pub async fn seed_inactive_api_token(
    db: &DatabaseConnection,
    permissions: serde_json::Value,
) -> String {
    let minted = mint_api_token();
    let perm_json = serde_json::to_string(&permissions).unwrap();

    exec(
        db,
        &format!(
            "INSERT INTO api_tokens (id, name, token_hash, token_prefix, permissions, is_active) \
             VALUES (gen_random_uuid(), 'test-token-inactive', '{}', '{}', '{perm_json}'::jsonb, false)",
            minted.token_hash, minted.token_prefix
        ),
    )
    .await;

    minted.raw_token
}

// ============================================================================
// Stream helpers (new)
// ============================================================================

pub async fn seed_data_stream(
    db: &DatabaseConnection,
    stream_id: &str,
    source_system: &str,
    source_key: &str,
) {
    exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', '{source_system}', '{source_key}', 'Test Stream', true)"
        ),
    )
    .await;
}

pub async fn seed_paired_stream(
    db: &DatabaseConnection,
    stream_id: &str,
    source_system: &str,
    source_key: &str,
    site_parameter_id: &str,
) {
    exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, site_parameter_id, paired_at, is_active) \
             VALUES ('{stream_id}', '{source_system}', '{source_key}', 'Test Paired Stream', '{site_parameter_id}', NOW(), true)"
        ),
    )
    .await;
}

pub async fn seed_sync_credentials(
    db: &DatabaseConnection,
    client_id: &str,
    client_secret: &str,
    service_type: &str,
) {
    let secret_hash = hash_token(client_secret);
    exec(
        db,
        &format!(
            "INSERT INTO sync_service_credentials (id, client_id, client_secret_hash, service_type) \
             VALUES (gen_random_uuid(), '{client_id}', '{secret_hash}', '{service_type}')"
        ),
    )
    .await;
}

/// Seed an unpaired data stream carrying a `metadata.hierarchy` (so `extract_hierarchy`/`create_plan`
/// resolve project/site/parameter), plus `source_path`/`source_name` for `grouped_discovery`, and
/// `n_readings` unpaired readings (`site_id`/`parameter_id` NULL) for apply-backfill to claim.
/// `coords` is `(latitude, longitude, altitude_m)`.
#[allow(clippy::too_many_arguments)]
pub async fn seed_unpaired_stream_with_hierarchy(
    db: &DatabaseConnection,
    stream_id: &str,
    source_system: &str,
    source_key: &str,
    project: &str,
    site: &str,
    parameter: &str,
    units: &str,
    coords: Option<(f64, f64, f64)>,
    n_readings: usize,
) {
    let coords_json = match coords {
        Some((lat, lon, alt)) => format!(
            ", \"coordinates\": {{\"latitude\": {lat}, \"longitude\": {lon}, \"altitude_m\": {alt}}}"
        ),
        None => String::new(),
    };
    let metadata = format!(
        "{{\"hierarchy\": {{\"project\": \"{project}\", \"site\": \"{site}\", \"parameter\": \"{parameter}\"}}, \"units\": \"{units}\"{coords_json}}}"
    );
    let source_path = format!("{source_system}/{project}/{site}/{parameter}");
    let source_name = format!("{site} - {parameter}");
    exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, source_path, metadata, is_active) \
             VALUES ('{stream_id}', '{source_system}', '{source_key}', '{source_name}', '{source_path}', '{metadata}'::jsonb, true)"
        ),
    )
    .await;

    if n_readings > 0 {
        let base = Utc::now() - Duration::days(2);
        let values: Vec<String> = (0..n_readings)
            .map(|i| {
                let t = (base + Duration::minutes((i as i64) * 10)).to_rfc3339();
                format!("('{stream_id}', '{t}', {})", 10.0 + i as f64)
            })
            .collect();
        exec(
            db,
            &format!(
                "INSERT INTO readings (stream_id, time, raw_value) VALUES {} ON CONFLICT DO NOTHING",
                values.join(", ")
            ),
        )
        .await;
    }
}
