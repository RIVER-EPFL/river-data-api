use axum::{
    Json,
    extract::{Path, Query, State},
    response::Response,
};
use chrono::{DateTime, NaiveDateTime, Utc};
use sea_orm::sea_query::{Alias, Expr, Order, PostgresQueryBuilder, Query as SeaQuery};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::cache;
use crate::common::cache_key;
use crate::common::series::{self, Cells, Table};
use crate::error::{AppError, AppResult};
use crate::routes::private::sites::aggregates::resolution_of;
use crate::routes::public::service::{
    PublicProjectConfig, PublicSiteConfig, get_public_config,
};

/// What the public tier treats as this site's data: one canonical row per timestamp, and nothing an
/// operator has flagged. The site-detail count and the readings query share this predicate so the
/// count, the series and the rollups cannot disagree about what the site serves.
const SERVED_READINGS: &str = "r.replicate_index = 0 AND r.is_flagged IS NOT TRUE";

// Time Format

const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

fn format_time(dt: DateTime<Utc>) -> String {
    dt.format(TIME_FORMAT).to_string()
}

fn parse_time(s: &str) -> Result<DateTime<Utc>, AppError> {
    if let Ok(naive) = NaiveDateTime::parse_from_str(s, TIME_FORMAT) {
        return Ok(naive.and_utc());
    }
    s.parse::<DateTime<Utc>>()
        .map_err(|e| AppError::BadRequest(format!("Invalid datetime '{s}': {e}")))
}

// Shared Types

#[derive(Debug, Serialize, ToSchema)]
pub struct SiteRef {
    pub code: String,
    pub name: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ParameterInfo {
    /// Stable parameter code (catalog `code`, e.g. "DOmgL").
    pub code: String,
    /// Human-readable parameter name (e.g. "Dissolved Oxygen").
    pub name: String,
    pub units: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// Resolution Helpers

fn resolve_site_from_config<'a>(
    config: &'a PublicProjectConfig,
    site_code: &str,
) -> AppResult<&'a PublicSiteConfig> {
    config
        .sites
        .iter()
        .find(|s| s.code.eq_ignore_ascii_case(site_code))
        .ok_or_else(|| AppError::NotFound(format!("Unknown site: {site_code}")))
}

/// Build the list of all public parameter names from config.
fn all_public_param_names(config: &PublicProjectConfig) -> Vec<String> {
    let mut names: Vec<String> = config
        .exposed_params
        .iter()
        .map(|ep| ep.code.clone())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Resolve public site_parameters at a site from the cached config.
/// Returns one `ResolvedParam` per exposed param belonging to this site.
fn resolve_site_parameters(
    site_id: Uuid,
    config: &PublicProjectConfig,
) -> Vec<ResolvedParam> {
    let mut resolved: Vec<ResolvedParam> = config
        .exposed_params
        .iter()
        .filter(|ep| ep.site_id == site_id)
        .map(|ep| ResolvedParam {
            site_id: ep.site_id,
            parameter_id: ep.parameter_id,
            code: ep.code.clone(),
            name: ep.name.clone(),
            units: ep.units.clone(),
        })
        .collect();
    resolved.sort_by(|a, b| a.code.cmp(&b.code));
    resolved
}

/// The parameter codes this site exposes, in resolution order, deduplicated (several
/// site_parameters can point at one catalog parameter).
///
/// Both series endpoints index their output by this list, so the readings and aggregates
/// endpoints report the same parameter set for a site and neither can emit a code the site does
/// not expose.
fn served_codes(resolved: &[&ResolvedParam]) -> Vec<String> {
    let mut codes: Vec<String> = Vec::new();
    for rp in resolved {
        if !codes.contains(&rp.code) {
            codes.push(rp.code.clone());
        }
    }
    codes
}

#[derive(Debug, Clone)]
struct ResolvedParam {
    site_id: Uuid,
    parameter_id: Uuid,
    code: String,
    name: String,
    units: String,
}

/// Parse the `parameters` query string and resolve each requested entry against the
/// project's exposed parameter codes. Matching is case-insensitive (forgiving for
/// hand-typed queries), but each match is normalized back to the canonical stored
/// `code` so downstream filtering and cache keys stay exact and stable.
fn resolve_requested_param_names(
    parameters: Option<&str>,
    config: &PublicProjectConfig,
) -> AppResult<Vec<String>> {
    let all_names = all_public_param_names(config);

    if all_names.is_empty() {
        return Ok(Vec::new());
    }

    let requested: Vec<String> = if let Some(params_str) = parameters {
        let mut resolved = Vec::new();
        for raw in params_str
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            match all_names.iter().find(|n| n.eq_ignore_ascii_case(raw)) {
                Some(canonical) => resolved.push(canonical.clone()),
                None => {
                    return Err(AppError::BadRequest(format!(
                        "Unknown parameter: {raw}. Available: {}",
                        all_names.join(", ")
                    )));
                }
            }
        }
        resolved
    } else {
        all_names
    };

    Ok(requested)
}

// GET /{project_code}/sites -- List all sites

/// List available sites for a public project.
#[utoipa::path(
    get,
    path = "/api/public/{project_code}/sites",
    params(
        ("project_code" = String, Path, description = "Public project code"),
    ),
    responses(
        (status = 200, body = Vec<SiteRef>),
        (status = 404, description = "Project not found"),
    ),
)]
pub async fn list_sites(
    State(state): State<AppState>,
    Path(project_code): Path<String>,
) -> AppResult<Json<Vec<SiteRef>>> {
    let config = get_public_config(&state.db, &state.public_config_cache, &project_code).await?;

    let sites: Vec<SiteRef> = config
        .sites
        .iter()
        .map(|s| SiteRef {
            code: s.code.clone(),
            name: s.name.clone(),
        })
        .collect();

    Ok(Json(sites))
}

// GET /{project_code}/sites/{site_code} -- Site info

#[derive(Debug, Serialize, ToSchema)]
pub struct SiteDetailResponse {
    pub site: SiteRef,
    pub parameters: Vec<ParameterInfo>,
    pub data_start: Option<String>,
    pub data_end: Option<String>,
    pub reading_count: i64,
}

#[derive(Debug, FromQueryResult)]
struct DataRangeRow {
    min_time: Option<DateTime<Utc>>,
    max_time: Option<DateTime<Utc>>,
    count: i64,
}

/// Site overview with available parameters and data range.
#[utoipa::path(
    get,
    path = "/api/public/{project_code}/sites/{site_code}",
    params(
        ("project_code" = String, Path, description = "Public project code"),
        ("site_code" = String, Path, description = "Site code"),
    ),
    responses(
        (status = 200, body = SiteDetailResponse),
        (status = 404, description = "Site not found"),
    ),
)]
pub async fn get_site(
    State(state): State<AppState>,
    Path((project_code, site_code)): Path<(String, String)>,
) -> AppResult<Json<SiteDetailResponse>> {
    let config = get_public_config(&state.db, &state.public_config_cache, &project_code).await?;
    let site = resolve_site_from_config(&config, &site_code)?;

    let resolved = resolve_site_parameters(site.site_id, &config);

    let params: Vec<ParameterInfo> = resolved
        .iter()
        .map(|rp| ParameterInfo {
            code: rp.code.clone(),
            name: rp.name.clone(),
            units: rp.units.clone(),
            description: None,
        })
        .collect();
    let param_ids: Vec<Uuid> = resolved.iter().map(|rp| rp.parameter_id).collect();

    let (data_start, data_end, reading_count) = if param_ids.is_empty() {
        (None, None, 0)
    } else {
        // param_ids are global parameter IDs; also filter by site_id
        let mut placeholders_parts: Vec<String> = Vec::new();
        for (i, _) in param_ids.iter().enumerate() {
            placeholders_parts.push(format!("${}", i + 2));
        }
        let placeholders = placeholders_parts.join(", ");

        let sql = format!(
            "SELECT MIN(r.time) AS min_time, MAX(r.time) AS max_time, COUNT(*) AS count \
             FROM readings r \
             WHERE r.site_id = $1 AND r.parameter_id IN ({placeholders}) \
               AND {SERVED_READINGS}"
        );

        let mut values: Vec<sea_orm::Value> = vec![site.site_id.into()];
        for id in &param_ids {
            values.push((*id).into());
        }

        let stmt = Statement::from_sql_and_values(sea_orm::DatabaseBackend::Postgres, &sql, values);

        let range = state
            .db
            .query_one(stmt)
            .await?
            .and_then(|row| DataRangeRow::from_query_result(&row, "").ok());

        range.map_or((None, None, 0), |r| {
            (
                r.min_time.map(format_time),
                r.max_time.map(format_time),
                r.count,
            )
        })
    };

    Ok(Json(SiteDetailResponse {
        site: SiteRef {
            code: site.code.clone(),
            name: site.name.clone(),
        },
        parameters: params,
        data_start,
        data_end,
        reading_count,
    }))
}

// GET /{project_code}/sites/{site_code}/parameters -- Parameter listing

/// List available parameters for a site in a public project.
#[utoipa::path(
    get,
    path = "/api/public/{project_code}/sites/{site_code}/parameters",
    params(
        ("project_code" = String, Path, description = "Public project code"),
        ("site_code" = String, Path, description = "Site code"),
    ),
    responses(
        (status = 200, body = Vec<ParameterInfo>),
        (status = 404, description = "Site not found"),
    ),
)]
pub async fn list_parameters(
    State(state): State<AppState>,
    Path((project_code, site_code)): Path<(String, String)>,
) -> AppResult<Json<Vec<ParameterInfo>>> {
    let config = get_public_config(&state.db, &state.public_config_cache, &project_code).await?;
    let site = resolve_site_from_config(&config, &site_code)?;

    let resolved = resolve_site_parameters(site.site_id, &config);
    let params: Vec<ParameterInfo> = resolved
        .iter()
        .map(|rp| ParameterInfo {
            code: rp.code.clone(),
            name: rp.name.clone(),
            units: rp.units.clone(),
            description: None,
        })
        .collect();

    Ok(Json(params))
}

// GET /{project_code}/sites/{site_code}/readings -- Raw time-series

#[derive(Debug, Deserialize, Serialize, IntoParams)]
pub struct ReadingsQuery {
    /// Format: YYYY-MM-DD HH:MM:SS or ISO 8601.
    pub start: Option<String>,
    /// Format: YYYY-MM-DD HH:MM:SS or ISO 8601.
    pub end: Option<String>,
    /// Comma-separated list of parameter public names. Omit for all.
    pub parameters: Option<String>,
    /// Filter by cadence: continuous (includes untagged legacy rows), spot (grab/lab), or
    /// derived. Omit for all readings.
    pub measurement_type: Option<String>,
    /// Include a per-point measurement_type array (continuous/spot/derived) on each parameter.
    #[serde(default)]
    pub include_measurement_type: Option<bool>,
    /// json (default), csv, or ndjson.
    ///
    /// Deliberately outside the cache key: the public tier caches the fetched data, and every
    /// format is rendered from that one entry.
    #[serde(default = "crate::common::bulk::default_format", skip_serializing)]
    pub format: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReadingsResponse {
    pub site: SiteRef,
    /// Earliest timestamp in the result.
    pub start: Option<String>,
    /// Latest timestamp in the result.
    pub end: Option<String>,
    /// Shared time axis for all parameters.
    pub times: Vec<String>,
    pub parameters: Vec<ParameterData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ParameterData {
    /// Stable parameter code (catalog `code`), also the CSV/NDJSON column key.
    pub code: String,
    pub name: String,
    pub units: String,
    /// One value per entry in `times`. Null where no reading exists.
    pub values: Vec<Option<f64>>,
    /// Per-point measurement type (continuous/spot/derived), aligned with `values`.
    /// Only present when `include_measurement_type=true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measurement_types: Option<Vec<Option<String>>>,
}

/// The public readings export: the value column per parameter, plus the per-point cadence when
/// the caller opted into it. Built from the same structs the JSON body serialises.
fn readings_table(times: &[String], params: &[ParameterData]) -> Table {
    let mut table = Table::new(times.to_vec());
    for p in params {
        table.column(p.code.clone(), Cells::Float(p.values.clone()));
    }
    if params.iter().any(|p| p.measurement_types.is_some()) {
        for p in params {
            table.column(
                format!("{}_measurement_type", p.code),
                Cells::Text(p.measurement_types.clone().unwrap_or_default()),
            );
        }
    }
    table
}

#[derive(Debug, FromQueryResult)]
struct ReadingRow {
    param_id: String,
    time: chrono::DateTime<chrono::FixedOffset>,
    value: f64,
    measurement_type: Option<String>,
}

/// Raw time-series readings for a public project site.
#[utoipa::path(
    get,
    path = "/api/public/{project_code}/sites/{site_code}/readings",
    params(
        ("project_code" = String, Path, description = "Public project code"),
        ("site_code" = String, Path, description = "Site code"),
        ReadingsQuery,
    ),
    responses(
        (status = 200, body = ReadingsResponse),
        (status = 400, description = "Invalid parameters"),
        (status = 404, description = "Site not found"),
    ),
)]
pub async fn get_readings(
    State(state): State<AppState>,
    Path((project_code, site_code)): Path<(String, String)>,
    Query(query): Query<ReadingsQuery>,
) -> AppResult<Response> {
    let config = get_public_config(&state.db, &state.public_config_cache, &project_code).await?;
    let site = resolve_site_from_config(&config, &site_code)?;

    let start_parsed = query.start.as_deref().map(parse_time).transpose()?;
    let end_parsed = query.end.as_deref().map(parse_time).transpose()?;

    // No start ⇒ default to a recent window rather than all history (the implicit
    // guard against unbounded pulls). No wall-clock cap: the rate limiter + cache bound cost.
    let default_lookback = state.config.default_readings_lookback_days;
    let effective_start = start_parsed
        .unwrap_or_else(|| chrono::Utc::now() - chrono::Duration::days(default_lookback));

    if let Some(e) = end_parsed
        && e < effective_start
    {
        return Err(AppError::BadRequest(
            "end time must not be before start time".to_string(),
        ));
    }

    let start = Some(effective_start);
    let end = end_parsed;
    let format = query.format.to_lowercase();
    let measurement_type = query.measurement_type.as_deref().unwrap_or("");
    if !measurement_type.is_empty() && !matches!(measurement_type, "continuous" | "spot" | "derived")
    {
        return Err(AppError::BadRequest(format!(
            "invalid measurement_type '{measurement_type}' (expected continuous, spot, or derived)"
        )));
    }
    let include_measurement_type = query.include_measurement_type.unwrap_or(false);

    let requested_names = resolve_requested_param_names(query.parameters.as_deref(), &config)?;

    if requested_names.is_empty() {
        return readings_response_from_data(site, Vec::new(), Vec::new(), &format, false).await;
    }

    // Resolve DB parameters matching the requested names
    let all_resolved = resolve_site_parameters(site.site_id, &config);
    let resolved: Vec<&ResolvedParam> = all_resolved
        .iter()
        .filter(|rp| requested_names.contains(&rp.code))
        .collect();
    let param_ids: Vec<Uuid> = {
        let mut ids: Vec<Uuid> = resolved.iter().map(|rp| rp.parameter_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };

    // Serve from the response cache when possible (any format), so repeat queries don't re-hit the
    // DB. The key carries the canonical parameter set plus every query field bar the output
    // format, which is rendered from the one cached entry.
    let mut names_key = requested_names.clone();
    names_key.sort();
    let cache_key = cache_key::key_for(
        &format!("pub_readings:{project_code}:{}", site.code),
        &ReadingsCacheKey {
            resolved_names: &names_key,
            effective_start,
            effective_end: end,
            query: &query,
        },
    );

    if let Some(bytes) = cache::get_cached(&state, &cache_key, &param_ids, end).await
        && let Ok(cached) = serde_json::from_slice::<CachedReadings>(&bytes)
    {
        return readings_response_from_data(site, cached.times, cached.parameters, &format, true)
            .await;
    }

    let (times_formatted, output_params) = fetch_readings(
        &state,
        &resolved,
        start,
        end,
        measurement_type,
        include_measurement_type,
    )
    .await?;

    let max_time = times_formatted.last().and_then(|s| parse_time(s).ok());
    if let Ok(bytes) = serde_json::to_vec(&CachedReadings {
        times: times_formatted.clone(),
        parameters: output_params.clone(),
    }) {
        cache::store_cached(&state, cache_key, bytes, max_time).await;
    }

    readings_response_from_data(site, times_formatted, output_params, &format, false).await
}

/// Everything that shapes a public readings body. The query is flattened in whole (bar the output
/// format), so a field added to `ReadingsQuery` enters the key by construction.
#[derive(Serialize)]
struct ReadingsCacheKey<'a> {
    resolved_names: &'a [String],
    effective_start: DateTime<Utc>,
    effective_end: Option<DateTime<Utc>>,
    #[serde(flatten)]
    query: &'a ReadingsQuery,
}

/// The same, for the public aggregates body.
#[derive(Serialize)]
struct AggregatesCacheKey<'a> {
    resolution: &'a str,
    resolved_names: &'a [String],
    #[serde(flatten)]
    query: &'a AggregatesQuery,
}

// GET /{project_code}/sites/{site_code}/aggregates/{resolution} -- Aggregated

#[derive(Debug, Deserialize, Serialize, IntoParams)]
pub struct AggregatesQuery {
    /// Format: YYYY-MM-DD HH:MM:SS or ISO 8601.
    pub start: String,
    /// Format: YYYY-MM-DD HH:MM:SS or ISO 8601.
    pub end: String,
    /// Comma-separated list of parameter public names. Omit for all.
    pub parameters: Option<String>,
    /// json (default), csv, or ndjson.
    ///
    /// Deliberately outside the cache key: the public tier caches the fetched data, and every
    /// format is rendered from that one entry.
    #[serde(default = "crate::common::bulk::default_format", skip_serializing)]
    pub format: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AggregatesResponse {
    pub site: SiteRef,
    pub resolution: String,
    pub start: String,
    pub end: String,
    /// Shared time axis for all parameters.
    pub times: Vec<String>,
    pub parameters: Vec<ParameterAggregateData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ParameterAggregateData {
    /// Stable parameter code (catalog `code`), also the CSV/NDJSON column key.
    pub code: String,
    pub name: String,
    pub units: String,
    pub avg: Vec<Option<f64>>,
    pub min: Vec<Option<f64>>,
    pub max: Vec<Option<f64>>,
    pub count: Vec<i64>,
}

/// The public aggregates export: the four statistics per parameter.
fn aggregates_table(times: &[String], params: &[ParameterAggregateData]) -> Table {
    let mut table = Table::new(times.to_vec());
    for p in params {
        table.column(format!("{}_avg", p.code), Cells::Float(p.avg.clone()));
        table.column(format!("{}_min", p.code), Cells::Float(p.min.clone()));
        table.column(format!("{}_max", p.code), Cells::Float(p.max.clone()));
        table.column(
            format!("{}_count", p.code),
            Cells::Int(p.count.iter().map(|c| Some(*c)).collect()),
        );
    }
    table
}

#[derive(Debug, FromQueryResult)]
struct AggregateRow {
    param_id: String,
    bucket: DateTime<Utc>,
    avg_value: Option<f64>,
    min_value: Option<f64>,
    max_value: Option<f64>,
    count: i64,
}

/// Aggregated time-series (hourly, daily, weekly, monthly) for a public project site.
/// Covers continuous and derived readings only; grab samples (measurement_type 'spot')
/// are excluded, fetch them from the readings endpoint with `measurement_type=spot`.
#[utoipa::path(
    get,
    path = "/api/public/{project_code}/sites/{site_code}/aggregates/{resolution}",
    params(
        ("project_code" = String, Path, description = "Public project code"),
        ("site_code" = String, Path, description = "Site code"),
        ("resolution" = String, Path, description = "hourly, daily, weekly, or monthly"),
        AggregatesQuery,
    ),
    responses(
        (status = 200, body = AggregatesResponse),
        (status = 400, description = "Invalid parameters"),
        (status = 404, description = "Site not found"),
    ),
)]
pub async fn get_aggregates(
    State(state): State<AppState>,
    Path((project_code, site_code, resolution)): Path<(String, String, String)>,
    Query(query): Query<AggregatesQuery>,
) -> AppResult<Response> {
    let config = get_public_config(&state.db, &state.public_config_cache, &project_code).await?;
    let site = resolve_site_from_config(&config, &site_code)?;

    let Some(rollup) = resolution_of(resolution.as_str()) else {
        return Err(AppError::BadRequest(format!(
            "Invalid resolution: {resolution}. Must be: hourly, daily, weekly, monthly"
        )));
    };

    let start = parse_time(&query.start)?;
    let end = parse_time(&query.end)?;
    if end < start {
        return Err(AppError::BadRequest(
            "end time must not be before start time".to_string(),
        ));
    }

    let format = query.format.to_lowercase();
    let requested_names = resolve_requested_param_names(query.parameters.as_deref(), &config)?;

    // Resolve DB parameters matching the requested names. The site's own resolution decides both
    // the query and the output index, so a parameter another site in the project exposes cannot
    // appear here as an all-null series.
    let all_resolved = resolve_site_parameters(site.site_id, &config);
    let resolved: Vec<&ResolvedParam> = all_resolved
        .iter()
        .filter(|rp| requested_names.contains(&rp.code))
        .collect();
    let codes = served_codes(&resolved);

    let param_ids: Vec<Uuid> = {
        let mut ids: Vec<Uuid> = resolved.iter().map(|rp| rp.parameter_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };

    if param_ids.is_empty() {
        return aggregates_response_from_data(
            site,
            &resolution,
            format_time(start),
            format_time(end),
            Vec::new(),
            Vec::new(),
            &format,
            false,
        )
        .await;
    }

    // Serve cached aggregate data when possible (bounded query ⇒ TTL-cached).
    let mut names_key = codes.clone();
    names_key.sort();
    let cache_key = cache_key::key_for(
        &format!("pub_aggregates:{project_code}:{}", site.code),
        &AggregatesCacheKey {
            resolution: &resolution,
            resolved_names: &names_key,
            query: &query,
        },
    );

    if let Some(bytes) = cache::get_cached(&state, &cache_key, &param_ids, Some(end)).await
        && let Ok(cached) = serde_json::from_slice::<CachedAggregates>(&bytes)
    {
        return aggregates_response_from_data(
            site,
            &resolution,
            format_time(start),
            format_time(end),
            cached.times,
            cached.parameters,
            &format,
            true,
        )
        .await;
    }

    let mut id_to_publics: HashMap<Uuid, Vec<(&str, &str)>> = HashMap::new();
    for rp in &resolved {
        id_to_publics
            .entry(rp.parameter_id)
            .or_default()
            .push((rp.code.as_str(), rp.units.as_str()));
    }

    // The CAGG is grouped by (bucket, site_id, parameter_id, sensor_id) since
    // m20260603_000007. The public API has no sensor concept, so collapse the sensor dimension:
    // count-weighted avg = SUM(sum_value)/SUM(count), MIN/MAX, SUM(count). Output shape and
    // ordering (param_id as TEXT, ordered by parameter code) are preserved.
    // $1 = site_id, $2.. = parameter_ids, then start, end.
    let id_placeholders: Vec<String> = param_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("${}", i + 2))
        .collect();
    let start_idx = param_ids.len() + 2;
    let end_idx = start_idx + 1;
    let sql = format!(
        r"
        SELECT
            p.id::text AS param_id,
            a.bucket AS bucket,
            CASE WHEN SUM(a.count) > 0 THEN SUM(a.sum_value) / SUM(a.count) ELSE NULL END AS avg_value,
            MIN(a.min_value) AS min_value,
            MAX(a.max_value) AS max_value,
            SUM(a.count)::bigint AS count
        FROM {view} a
        JOIN parameters p ON a.parameter_id = p.id
        WHERE a.site_id = $1
          AND p.id IN ({ids})
          AND a.bucket >= ${start_idx}
          AND a.bucket <= ${end_idx}
        GROUP BY a.bucket, p.id, p.code
        ORDER BY a.bucket ASC, p.code ASC
        ",
        view = rollup.view(),
        ids = id_placeholders.join(","),
    );
    let mut values: Vec<sea_orm::Value> = vec![site.site_id.into()];
    values.extend(param_ids.iter().map(|id| (*id).into()));
    values.push(start.into());
    values.push(end.into());

    let stmt = Statement::from_sql_and_values(sea_orm::DatabaseBackend::Postgres, &sql, values);

    let rows: Vec<AggregateRow> = state
        .db
        .query_all(stmt)
        .await?
        .into_iter()
        .filter_map(|row| AggregateRow::from_query_result(&row, "").ok())
        .collect();

    // Collect unique buckets and group by public parameter name
    let mut times_ordered: Vec<DateTime<Utc>> = Vec::new();
    let mut time_set: std::collections::HashSet<DateTime<Utc>> = std::collections::HashSet::new();

    struct AggValues {
        avg: Option<f64>,
        min: Option<f64>,
        max: Option<f64>,
        count: i64,
    }

    let mut param_aggs: HashMap<String, Vec<(DateTime<Utc>, AggValues)>> = HashMap::new();

    for row in &rows {
        if time_set.insert(row.bucket) {
            times_ordered.push(row.bucket);
        }
        let param_uuid = row.param_id.parse::<Uuid>().ok();
        if let Some(configs) = param_uuid.and_then(|uuid| id_to_publics.get(&uuid)) {
            for (name, _units) in configs {
                param_aggs.entry(name.to_string()).or_default().push((
                    row.bucket,
                    AggValues {
                        avg: row.avg_value,
                        min: row.min_value,
                        max: row.max_value,
                        count: row.count,
                    },
                ));
            }
        }
    }

    times_ordered.sort_unstable();

    let time_index: HashMap<DateTime<Utc>, usize> = times_ordered
        .iter()
        .enumerate()
        .map(|(i, t)| (*t, i))
        .collect();

    let num_times = times_ordered.len();

    // One series per code the site exposes, the same index the readings endpoint builds.
    let mut output_params: Vec<ParameterAggregateData> = Vec::new();

    for code in &codes {
        let matched = resolved.iter().find(|rp| &rp.code == code);
        let name = matched.map_or("", |rp| rp.name.as_str());
        let units = matched.map_or("", |rp| rp.units.as_str());

        let mut avg = vec![None; num_times];
        let mut min = vec![None; num_times];
        let mut max = vec![None; num_times];
        let mut count = vec![0i64; num_times];

        if let Some(aggs) = param_aggs.get(code.as_str()) {
            for (bucket, agg) in aggs {
                if let Some(&idx) = time_index.get(bucket) {
                    avg[idx] = agg.avg;
                    min[idx] = agg.min;
                    max[idx] = agg.max;
                    count[idx] = agg.count;
                }
            }
        }

        output_params.push(ParameterAggregateData {
            code: code.clone(),
            name: name.to_string(),
            units: units.to_string(),
            avg,
            min,
            max,
            count,
        });
    }

    let times_formatted: Vec<String> = times_ordered.iter().map(|t| format_time(*t)).collect();

    let max_time = times_ordered.last().copied();
    if let Ok(bytes) = serde_json::to_vec(&CachedAggregates {
        times: times_formatted.clone(),
        parameters: output_params.clone(),
    }) {
        cache::store_cached(&state, cache_key, bytes, max_time).await;
    }

    aggregates_response_from_data(
        site,
        &resolution,
        format_time(start),
        format_time(end),
        times_formatted,
        output_params,
        &format,
        false,
    )
    .await
}

// Response Cache Payloads + Format Helpers

// We cache the fetched data (times + parameters) so every output format is served
// without re-querying the database on repeat requests.
#[derive(Serialize, Deserialize)]
struct CachedReadings {
    times: Vec<String>,
    parameters: Vec<ParameterData>,
}

#[derive(Serialize, Deserialize)]
struct CachedAggregates {
    times: Vec<String>,
    parameters: Vec<ParameterAggregateData>,
}

async fn readings_response_from_data(
    site: &PublicSiteConfig,
    times: Vec<String>,
    parameters: Vec<ParameterData>,
    format: &str,
    cache_hit: bool,
) -> AppResult<Response> {
    series::respond(
        format,
        (times, parameters),
        |(times, params)| readings_table(times, params),
        |(times, parameters)| async move {
            let start = times.first().cloned();
            let end = times.last().cloned();
            let response = ReadingsResponse {
                site: SiteRef {
                    code: site.code.clone(),
                    name: site.name.clone(),
                },
                start,
                end,
                times,
                parameters,
            };
            let bytes =
                serde_json::to_vec(&response).map_err(|e| AppError::Internal(e.to_string()))?;
            cache::json_response(bytes, cache_hit)
        },
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn aggregates_response_from_data(
    site: &PublicSiteConfig,
    resolution: &str,
    start: String,
    end: String,
    times: Vec<String>,
    parameters: Vec<ParameterAggregateData>,
    format: &str,
    cache_hit: bool,
) -> AppResult<Response> {
    series::respond(
        format,
        (times, parameters),
        |(times, params)| aggregates_table(times, params),
        |(times, parameters)| async move {
            let response = AggregatesResponse {
                site: SiteRef {
                    code: site.code.clone(),
                    name: site.name.clone(),
                },
                resolution: resolution.to_string(),
                start,
                end,
                times,
                parameters,
            };
            let bytes =
                serde_json::to_vec(&response).map_err(|e| AppError::Internal(e.to_string()))?;
            cache::json_response(bytes, cache_hit)
        },
    )
    .await
}

// Shared Helpers

/// Fetch raw readings for resolved parameters, build time axis and parameter arrays.
async fn fetch_readings(
    state: &AppState,
    resolved: &[&ResolvedParam],
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    measurement_type: &str,
    include_measurement_type: bool,
) -> AppResult<(Vec<String>, Vec<ParameterData>)> {
    let param_ids: Vec<Uuid> = {
        let mut ids: Vec<Uuid> = resolved.iter().map(|rp| rp.parameter_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };

    if param_ids.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut id_to_publics: HashMap<Uuid, Vec<(&str, &str)>> = HashMap::new();
    for rp in resolved {
        id_to_publics
            .entry(rp.parameter_id)
            .or_default()
            .push((rp.code.as_str(), rp.units.as_str()));
    }

    // All resolved params come from the same site
    let site_id = resolved
        .first()
        .map(|rp| rp.site_id)
        .ok_or_else(|| AppError::NotFound("No resolved parameters found".to_string()))?;

    let r = Alias::new("r");
    let p = Alias::new("p");

    let mut query = SeaQuery::select();
    query
        .expr_as(
            Expr::col((p.clone(), Alias::new("id"))).cast_as(Alias::new("TEXT")),
            Alias::new("param_id"),
        )
        .column((r.clone(), Alias::new("time")))
        .expr_as(
            // A replicate group is served as its sample mean (same as the private endpoint),
            // so a replicated timestamp yields one deterministic value.
            Expr::cust("COALESCE(smp.mean, r.calibrated_value, r.raw_value)"),
            Alias::new("value"),
        )
        .column((r.clone(), Alias::new("measurement_type")))
        .from_as(Alias::new("readings"), r.clone())
        .join_as(
            sea_orm::sea_query::JoinType::Join,
            Alias::new("parameters"),
            p.clone(),
            Expr::col((r.clone(), Alias::new("parameter_id")))
                .equals((p.clone(), Alias::new("id"))),
        )
        .join_as(
            sea_orm::sea_query::JoinType::LeftJoin,
            Alias::new("samples"),
            Alias::new("smp"),
            Expr::col((Alias::new("smp"), Alias::new("id")))
                .equals((r.clone(), Alias::new("sample_id"))),
        )
        .and_where(Expr::col((r.clone(), Alias::new("site_id"))).eq(site_id))
        .and_where(Expr::cust(SERVED_READINGS))
        .and_where(Expr::col((p.clone(), Alias::new("id"))).is_in(param_ids.clone()))
        .order_by((r.clone(), Alias::new("time")), Order::Asc)
        .order_by((p.clone(), Alias::new("code")), Order::Asc);

    if let Some(s) = start {
        query.and_where(Expr::col((r.clone(), Alias::new("time"))).gte(s));
    }
    if let Some(e) = end {
        query.and_where(Expr::col((r.clone(), Alias::new("time"))).lte(e));
    }
    // Same semantics as the private readings filter: 'continuous' includes legacy NULL rows.
    match measurement_type {
        "" => {}
        "continuous" => {
            query.and_where(Expr::cust(
                "(r.measurement_type = 'continuous' OR r.measurement_type IS NULL)",
            ));
        }
        other => {
            query.and_where(Expr::col((r.clone(), Alias::new("measurement_type"))).eq(other));
        }
    }

    let (sql, values) = query.build(PostgresQueryBuilder);
    let stmt = Statement::from_sql_and_values(sea_orm::DatabaseBackend::Postgres, sql, values.0);

    let rows: Vec<ReadingRow> = state
        .db
        .query_all(stmt)
        .await?
        .into_iter()
        .filter_map(|row| ReadingRow::from_query_result(&row, "").ok())
        .collect();

    let mut times_ordered: Vec<DateTime<Utc>> = Vec::new();
    let mut time_set: std::collections::HashSet<DateTime<Utc>> = std::collections::HashSet::new();
    // Map from parameter name -> Vec<(time, value, measurement_type)>
    let mut param_values: HashMap<String, Vec<(DateTime<Utc>, f64, Option<String>)>> =
        HashMap::new();

    for row in &rows {
        let time = row.time.with_timezone(&Utc);
        if time_set.insert(time) {
            times_ordered.push(time);
        }

        let param_uuid = row.param_id.parse::<Uuid>().ok();
        if let Some(configs) = param_uuid.and_then(|uuid| id_to_publics.get(&uuid)) {
            for (name, _units) in configs {
                param_values.entry(name.to_string()).or_default().push((
                    time,
                    row.value,
                    row.measurement_type.clone(),
                ));
            }
        }
    }

    times_ordered.sort_unstable();

    let time_index: HashMap<DateTime<Utc>, usize> = times_ordered
        .iter()
        .enumerate()
        .map(|(i, t)| (*t, i))
        .collect();

    let num_times = times_ordered.len();

    let mut output_params: Vec<ParameterData> = Vec::new();
    for code in &served_codes(resolved) {
        let matched = resolved.iter().find(|rp| &rp.code == code);
        let name = matched.map_or("", |rp| rp.name.as_str());
        let units = matched.map_or("", |rp| rp.units.as_str());

        let mut values = vec![None; num_times];
        let mut measurement_types = if include_measurement_type {
            Some(vec![None::<String>; num_times])
        } else {
            None
        };
        if let Some(readings) = param_values.get(code.as_str()) {
            for (time, value, mt) in readings {
                if let Some(&idx) = time_index.get(time) {
                    values[idx] = Some(*value);
                    if let Some(mts) = measurement_types.as_mut() {
                        // NULL legacy rows read as continuous, matching the filter semantics.
                        mts[idx] =
                            Some(mt.clone().unwrap_or_else(|| "continuous".to_string()));
                    }
                }
            }
        }
        output_params.push(ParameterData {
            code: code.clone(),
            name: name.to_string(),
            units: units.to_string(),
            values,
            measurement_types,
        });
    }

    let times_formatted: Vec<String> = times_ordered.iter().map(|t| format_time(*t)).collect();
    Ok((times_formatted, output_params))
}

