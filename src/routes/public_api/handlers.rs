use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, NaiveDateTime, Utc};
use sea_orm::sea_query::{Alias, Expr, Order, PostgresQueryBuilder, Query as SeaQuery};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, Statement};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::site_parameters as site_parameters_entity;
use crate::error::{AppError, AppResult};
use crate::services::bulk::{self, StreamableAggregateParam, StreamableParam};
use crate::services::public_api_config::{
    get_public_config, ExposedParamConfig, PublicProjectConfig, PublicSiteConfig,
};

// ============================================================================
// Time Format
// ============================================================================

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

// ============================================================================
// Shared Types
// ============================================================================

fn default_format() -> String {
    "json".to_string()
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SiteRef {
    pub id: String,
    pub uuid: String,
    pub name: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ParameterInfo {
    pub name: String,
    pub units: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// ============================================================================
// Resolution Helpers
// ============================================================================

/// Resolve a site within a public project by slug or UUID.
fn resolve_site_from_config<'a>(
    config: &'a PublicProjectConfig,
    site_id: &str,
) -> AppResult<&'a PublicSiteConfig> {
    // Try UUID first
    if let Ok(uuid) = site_id.parse::<Uuid>() {
        return config
            .sites
            .iter()
            .find(|s| s.site_id == uuid)
            .ok_or_else(|| AppError::NotFound(format!("Site not found: {site_id}")));
    }
    // Try slug (case-insensitive)
    config
        .sites
        .iter()
        .find(|s| s.slug.eq_ignore_ascii_case(site_id))
        .ok_or_else(|| AppError::NotFound(format!("Unknown site: {site_id}")))
}

/// Build the list of all public parameter names from config (base + derived).
fn all_public_param_names(config: &PublicProjectConfig) -> Vec<String> {
    config
        .exposed_params
        .iter()
        .map(|ep| ep.public_name.clone())
        .collect()
}

/// Build parameter_id -> ExposedParamConfig lookup from the project config.
fn build_param_to_config_map(
    config: &PublicProjectConfig,
) -> HashMap<Uuid, &ExposedParamConfig> {
    config
        .exposed_params
        .iter()
        .map(|ep| (ep.parameter_id, ep))
        .collect()
}

/// Resolve which DB site_parameters at a site match the exposed config.
/// Returns a list of (site_id, parameter_id, public_name, public_units, is_derived).
async fn resolve_site_parameters(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    config: &PublicProjectConfig,
) -> AppResult<Vec<ResolvedParam>> {
    let param_to_config = build_param_to_config_map(config);
    let param_ids: Vec<Uuid> = param_to_config.keys().copied().collect();

    if param_ids.is_empty() {
        return Ok(Vec::new());
    }

    // Find all site_parameters at this site whose parameter_id matches an exposed config
    let params = site_parameters_entity::Entity::find()
        .filter(site_parameters_entity::Column::SiteId.eq(site_id))
        .filter(site_parameters_entity::Column::ParameterId.is_in(param_ids))
        .all(db)
        .await
        .map_err(AppError::Database)?;

    let mut resolved = Vec::new();
    for sp in params {
        let is_derived = sp.is_derived.unwrap_or(false);

        if let Some(ep_config) = param_to_config.get(&sp.parameter_id) {
            // Non-derived params are always included
            // Derived params are included only if the config has include_derived=true
            if !is_derived || ep_config.include_derived {
                resolved.push(ResolvedParam {
                    site_id: sp.site_id,
                    parameter_id: sp.parameter_id,
                    public_name: ep_config.public_name.clone(),
                    public_units: ep_config.public_units.clone(),
                });
            }
        }
    }

    resolved.sort_by(|a, b| a.public_name.cmp(&b.public_name));
    Ok(resolved)
}

#[derive(Debug, Clone)]
struct ResolvedParam {
    site_id: Uuid,
    parameter_id: Uuid,
    public_name: String,
    public_units: String,
}

/// Parse the `parameters` query string and filter against the project's exposed params.
/// Returns the list of requested public names.
fn resolve_requested_param_names(
    parameters: Option<&str>,
    config: &PublicProjectConfig,
) -> AppResult<Vec<String>> {
    let all_names = all_public_param_names(config);

    if all_names.is_empty() {
        return Ok(Vec::new());
    }

    let requested: Vec<String> = if let Some(params_str) = parameters {
        let names: Vec<String> = params_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        // Validate that all requested names are in the exposed set
        for name in &names {
            if !all_names.iter().any(|n| n == name) {
                return Err(AppError::BadRequest(format!(
                    "Unknown parameter: {name}. Available: {}",
                    all_names.join(", ")
                )));
            }
        }
        names
    } else {
        all_names
    };

    Ok(requested)
}

// ============================================================================
// GET /{project_slug}/sites -- List all sites
// ============================================================================

/// List available sites for a public project.
#[utoipa::path(
    get,
    path = "/api/public/{project_slug}/sites",
    params(
        ("project_slug" = String, Path, description = "Public project slug"),
    ),
    responses(
        (status = 200, body = Vec<SiteRef>),
        (status = 404, description = "Project not found"),
    ),
)]
pub async fn list_sites(
    State(state): State<AppState>,
    Path(project_slug): Path<String>,
) -> AppResult<Json<Vec<SiteRef>>> {
    let config = get_public_config(&state.db, &state.public_config_cache, &project_slug).await?;

    let sites: Vec<SiteRef> = config
        .sites
        .iter()
        .map(|s| SiteRef {
            id: s.slug.clone(),
            uuid: s.site_id.to_string(),
            name: s.name.clone(),
        })
        .collect();

    Ok(Json(sites))
}

// ============================================================================
// GET /{project_slug}/sites/{site_id} -- Site info
// ============================================================================

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
    path = "/api/public/{project_slug}/sites/{site_id}",
    params(
        ("project_slug" = String, Path, description = "Public project slug"),
        ("site_id" = String, Path, description = "Site slug or UUID"),
    ),
    responses(
        (status = 200, body = SiteDetailResponse),
        (status = 404, description = "Site not found"),
    ),
)]
pub async fn get_site(
    State(state): State<AppState>,
    Path((project_slug, site_id)): Path<(String, String)>,
) -> AppResult<Json<SiteDetailResponse>> {
    let config = get_public_config(&state.db, &state.public_config_cache, &project_slug).await?;
    let site = resolve_site_from_config(&config, &site_id)?;

    let params: Vec<ParameterInfo> = config
        .exposed_params
        .iter()
        .map(|ep| ParameterInfo {
            name: ep.public_name.clone(),
            units: ep.public_units.clone(),
            description: ep.description.clone(),
        })
        .collect();

    // Query data range for exposed parameters at this site
    let resolved = resolve_site_parameters(&state.db, site.site_id, &config).await?;
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
             WHERE r.site_id = $1 AND r.parameter_id IN ({placeholders})"
        );

        let mut values: Vec<sea_orm::Value> = vec![site.site_id.into()];
        for id in &param_ids {
            values.push((*id).into());
        }

        let stmt = Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        );

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
            id: site.slug.clone(),
            uuid: site.site_id.to_string(),
            name: site.name.clone(),
        },
        parameters: params,
        data_start,
        data_end,
        reading_count,
    }))
}

// ============================================================================
// GET /{project_slug}/sites/{site_id}/parameters -- Parameter listing
// ============================================================================

/// List available parameters for a site in a public project.
#[utoipa::path(
    get,
    path = "/api/public/{project_slug}/sites/{site_id}/parameters",
    params(
        ("project_slug" = String, Path, description = "Public project slug"),
        ("site_id" = String, Path, description = "Site slug or UUID"),
    ),
    responses(
        (status = 200, body = Vec<ParameterInfo>),
        (status = 404, description = "Site not found"),
    ),
)]
pub async fn list_parameters(
    State(state): State<AppState>,
    Path((project_slug, site_id)): Path<(String, String)>,
) -> AppResult<Json<Vec<ParameterInfo>>> {
    let config = get_public_config(&state.db, &state.public_config_cache, &project_slug).await?;
    let _ = resolve_site_from_config(&config, &site_id)?;

    let params: Vec<ParameterInfo> = config
        .exposed_params
        .iter()
        .map(|ep| ParameterInfo {
            name: ep.public_name.clone(),
            units: ep.public_units.clone(),
            description: ep.description.clone(),
        })
        .collect();

    Ok(Json(params))
}

// ============================================================================
// GET /{project_slug}/sites/{site_id}/readings -- Raw time-series
// ============================================================================

#[derive(Debug, Deserialize, IntoParams)]
pub struct ReadingsQuery {
    /// Format: YYYY-MM-DD HH:MM:SS or ISO 8601.
    pub start: Option<String>,
    /// Format: YYYY-MM-DD HH:MM:SS or ISO 8601.
    pub end: Option<String>,
    /// Comma-separated list of parameter public names. Omit for all.
    pub parameters: Option<String>,
    /// json (default), csv, or ndjson.
    #[serde(default = "default_format")]
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

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ParameterData {
    pub name: String,
    pub units: String,
    /// One value per entry in `times`. Null where no reading exists.
    pub values: Vec<Option<f64>>,
}

impl StreamableParam for ParameterData {
    fn name(&self) -> &str {
        &self.name
    }
    fn value_at(&self, index: usize) -> Option<f64> {
        self.values.get(index).and_then(|v| *v)
    }
}

#[derive(Debug, FromQueryResult)]
struct ReadingRow {
    param_id: String,
    time: chrono::DateTime<chrono::FixedOffset>,
    value: f64,
}

/// Raw time-series readings for a public project site.
#[utoipa::path(
    get,
    path = "/api/public/{project_slug}/sites/{site_id}/readings",
    params(
        ("project_slug" = String, Path, description = "Public project slug"),
        ("site_id" = String, Path, description = "Site slug or UUID"),
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
    Path((project_slug, site_id)): Path<(String, String)>,
    Query(query): Query<ReadingsQuery>,
) -> AppResult<Response> {
    let config = get_public_config(&state.db, &state.public_config_cache, &project_slug).await?;
    let site = resolve_site_from_config(&config, &site_id)?;

    let start = query.start.as_deref().map(parse_time).transpose()?;
    let end = query.end.as_deref().map(parse_time).transpose()?;

    if let (Some(s), Some(e)) = (start, end) {
        if e <= s {
            return Err(AppError::BadRequest(
                "end time must be after start time".to_string(),
            ));
        }
    }

    let requested_names = resolve_requested_param_names(query.parameters.as_deref(), &config)?;

    if requested_names.is_empty() {
        // No exposed params configured: return empty response
        let response = ReadingsResponse {
            site: SiteRef {
                id: site.slug.clone(),
                uuid: site.site_id.to_string(),
                name: site.name.clone(),
            },
            start: None,
            end: None,
            times: Vec::new(),
            parameters: Vec::new(),
        };
        return Ok(Json(response).into_response());
    }

    // Resolve DB parameters matching the requested public names
    let all_resolved = resolve_site_parameters(&state.db, site.site_id, &config).await?;

    // Filter to only the requested public names
    let resolved: Vec<&ResolvedParam> = all_resolved
        .iter()
        .filter(|rp| requested_names.contains(&rp.public_name))
        .collect();

    let (times_formatted, output_params) =
        fetch_readings(&state, &resolved, start, end).await?;

    let actual_start = times_formatted.first().cloned();
    let actual_end = times_formatted.last().cloned();

    let format = query.format.to_lowercase();
    match format.as_str() {
        "csv" => build_csv_response(times_formatted, &output_params),
        "ndjson" => build_ndjson_response(times_formatted, &output_params),
        _ => {
            let response = ReadingsResponse {
                site: SiteRef {
                    id: site.slug.clone(),
                    uuid: site.site_id.to_string(),
                    name: site.name.clone(),
                },
                start: actual_start,
                end: actual_end,
                times: times_formatted,
                parameters: output_params,
            };
            Ok(Json(response).into_response())
        }
    }
}

// ============================================================================
// GET /{project_slug}/sites/{site_id}/aggregates/{resolution} -- Aggregated
// ============================================================================

#[derive(Debug, Deserialize, IntoParams)]
pub struct AggregatesQuery {
    /// Format: YYYY-MM-DD HH:MM:SS or ISO 8601.
    pub start: String,
    /// Format: YYYY-MM-DD HH:MM:SS or ISO 8601.
    pub end: String,
    /// Comma-separated list of parameter public names. Omit for all.
    pub parameters: Option<String>,
    /// json (default), csv, or ndjson.
    #[serde(default = "default_format")]
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

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ParameterAggregateData {
    pub name: String,
    pub units: String,
    pub avg: Vec<Option<f64>>,
    pub min: Vec<Option<f64>>,
    pub max: Vec<Option<f64>>,
    pub count: Vec<i64>,
}

impl StreamableAggregateParam for ParameterAggregateData {
    fn name(&self) -> &str {
        &self.name
    }
    fn avg_at(&self, index: usize) -> Option<f64> {
        self.avg.get(index).and_then(|v| *v)
    }
    fn min_at(&self, index: usize) -> Option<f64> {
        self.min.get(index).and_then(|v| *v)
    }
    fn max_at(&self, index: usize) -> Option<f64> {
        self.max.get(index).and_then(|v| *v)
    }
    fn count_at(&self, index: usize) -> Option<i64> {
        self.count.get(index).copied()
    }
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
#[utoipa::path(
    get,
    path = "/api/public/{project_slug}/sites/{site_id}/aggregates/{resolution}",
    params(
        ("project_slug" = String, Path, description = "Public project slug"),
        ("site_id" = String, Path, description = "Site slug or UUID"),
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
    Path((project_slug, site_id, resolution)): Path<(String, String, String)>,
    Query(query): Query<AggregatesQuery>,
) -> AppResult<Response> {
    let config = get_public_config(&state.db, &state.public_config_cache, &project_slug).await?;
    let site = resolve_site_from_config(&config, &site_id)?;

    let view_name = match resolution.as_str() {
        "hourly" => "readings_hourly",
        "daily" => "readings_daily",
        "weekly" => "readings_weekly",
        "monthly" => "readings_monthly",
        _ => {
            return Err(AppError::BadRequest(format!(
                "Invalid resolution: {resolution}. Must be: hourly, daily, weekly, monthly"
            )));
        }
    };

    let start = parse_time(&query.start)?;
    let end = parse_time(&query.end)?;
    if end <= start {
        return Err(AppError::BadRequest(
            "end time must be after start time".to_string(),
        ));
    }

    let requested_names = resolve_requested_param_names(query.parameters.as_deref(), &config)?;

    if requested_names.is_empty() {
        let response = AggregatesResponse {
            site: SiteRef {
                id: site.slug.clone(),
                uuid: site.site_id.to_string(),
                name: site.name.clone(),
            },
            resolution,
            start: format_time(start),
            end: format_time(end),
            times: Vec::new(),
            parameters: Vec::new(),
        };
        return Ok(Json(response).into_response());
    }

    // Resolve DB parameters matching the requested public names
    let all_resolved = resolve_site_parameters(&state.db, site.site_id, &config).await?;
    let resolved: Vec<&ResolvedParam> = all_resolved
        .iter()
        .filter(|rp| requested_names.contains(&rp.public_name))
        .collect();

    let param_ids: Vec<Uuid> = resolved.iter().map(|rp| rp.parameter_id).collect();

    if param_ids.is_empty() {
        let response = AggregatesResponse {
            site: SiteRef {
                id: site.slug.clone(),
                uuid: site.site_id.to_string(),
                name: site.name.clone(),
            },
            resolution,
            start: format_time(start),
            end: format_time(end),
            times: Vec::new(),
            parameters: Vec::new(),
        };
        return Ok(Json(response).into_response());
    }

    // Build a map from parameter_id -> (public_name, public_units)
    let id_to_public: HashMap<Uuid, (&str, &str)> = resolved
        .iter()
        .map(|rp| {
            (
                rp.parameter_id,
                (rp.public_name.as_str(), rp.public_units.as_str()),
            )
        })
        .collect();

    // Build parameterized query against continuous aggregate
    let view = Alias::new(view_name);
    let a = Alias::new("a");
    let p = Alias::new("p");

    let (sql, values) = SeaQuery::select()
        .expr_as(
            Expr::col((p.clone(), Alias::new("id"))).cast_as(Alias::new("TEXT")),
            Alias::new("param_id"),
        )
        .column((a.clone(), Alias::new("bucket")))
        .column((a.clone(), Alias::new("avg_value")))
        .column((a.clone(), Alias::new("min_value")))
        .column((a.clone(), Alias::new("max_value")))
        .column((a.clone(), Alias::new("count")))
        .from_as(view, a.clone())
        .join_as(
            sea_orm::sea_query::JoinType::Join,
            Alias::new("parameters"),
            p.clone(),
            Expr::col((a.clone(), Alias::new("parameter_id")))
                .equals((p.clone(), Alias::new("id"))),
        )
        .and_where(Expr::col((a.clone(), Alias::new("site_id"))).eq(site.site_id))
        .and_where(Expr::col((p.clone(), Alias::new("id"))).is_in(param_ids.clone()))
        .and_where(Expr::col((a.clone(), Alias::new("bucket"))).gte(start))
        .and_where(Expr::col((a.clone(), Alias::new("bucket"))).lte(end))
        .order_by((a.clone(), Alias::new("bucket")), Order::Asc)
        .order_by((p.clone(), Alias::new("name")), Order::Asc)
        .build(PostgresQueryBuilder);

    let stmt = Statement::from_sql_and_values(sea_orm::DatabaseBackend::Postgres, sql, values.0);

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
        // Map DB parameter_id back to public name
        let param_uuid = row.param_id.parse::<Uuid>().ok();
        let public_name = param_uuid
            .and_then(|uuid| id_to_public.get(&uuid))
            .map(|(name, _)| name.to_string())
            .unwrap_or_else(|| row.param_id.clone());

        param_aggs
            .entry(public_name)
            .or_default()
            .push((
                row.bucket,
                AggValues {
                    avg: row.avg_value,
                    min: row.min_value,
                    max: row.max_value,
                    count: row.count,
                },
            ));
    }

    times_ordered.sort_unstable();

    let time_index: HashMap<DateTime<Utc>, usize> = times_ordered
        .iter()
        .enumerate()
        .map(|(i, t)| (*t, i))
        .collect();

    let num_times = times_ordered.len();

    // Build output parameters in the order of requested names
    let mut output_params: Vec<ParameterAggregateData> = Vec::new();

    for name in &requested_names {
        let units = resolved
            .iter()
            .find(|rp| &rp.public_name == name)
            .map(|rp| rp.public_units.as_str())
            .unwrap_or("");

        let mut avg = vec![None; num_times];
        let mut min = vec![None; num_times];
        let mut max = vec![None; num_times];
        let mut count = vec![0i64; num_times];

        if let Some(aggs) = param_aggs.get(name.as_str()) {
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
            name: name.clone(),
            units: units.to_string(),
            avg,
            min,
            max,
            count,
        });
    }

    let times_formatted: Vec<String> = times_ordered.iter().map(|t| format_time(*t)).collect();

    let format = query.format.to_lowercase();
    match format.as_str() {
        "csv" => build_aggregates_csv(times_formatted, &output_params),
        "ndjson" => build_aggregates_ndjson(times_formatted, &output_params),
        _ => {
            let response = AggregatesResponse {
                site: SiteRef {
                    id: site.slug.clone(),
                    uuid: site.site_id.to_string(),
                    name: site.name.clone(),
                },
                resolution,
                start: format_time(start),
                end: format_time(end),
                times: times_formatted,
                parameters: output_params,
            };
            Ok(Json(response).into_response())
        }
    }
}

// ============================================================================
// Shared Helpers
// ============================================================================

/// Fetch raw readings for resolved parameters, build time axis and parameter arrays.
async fn fetch_readings(
    state: &AppState,
    resolved: &[&ResolvedParam],
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
) -> AppResult<(Vec<String>, Vec<ParameterData>)> {
    let param_ids: Vec<Uuid> = resolved.iter().map(|rp| rp.parameter_id).collect();

    if param_ids.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    // Build a map from parameter_id -> (public_name, public_units)
    let id_to_public: HashMap<Uuid, (&str, &str)> = resolved
        .iter()
        .map(|rp| {
            (
                rp.parameter_id,
                (rp.public_name.as_str(), rp.public_units.as_str()),
            )
        })
        .collect();

    // All resolved params come from the same site
    let site_id = resolved.first().map(|rp| rp.site_id).unwrap();

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
            Expr::cust("COALESCE(r.calibrated_value, r.raw_value)"),
            Alias::new("value"),
        )
        .from_as(Alias::new("readings"), r.clone())
        .join_as(
            sea_orm::sea_query::JoinType::Join,
            Alias::new("parameters"),
            p.clone(),
            Expr::col((r.clone(), Alias::new("parameter_id")))
                .equals((p.clone(), Alias::new("id"))),
        )
        .and_where(Expr::col((r.clone(), Alias::new("site_id"))).eq(site_id))
        .and_where(Expr::col((p.clone(), Alias::new("id"))).is_in(param_ids.clone()))
        .order_by((r.clone(), Alias::new("time")), Order::Asc)
        .order_by((p.clone(), Alias::new("name")), Order::Asc);

    if let Some(s) = start {
        query.and_where(Expr::col((r.clone(), Alias::new("time"))).gte(s));
    }
    if let Some(e) = end {
        query.and_where(Expr::col((r.clone(), Alias::new("time"))).lte(e));
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
    // Map from public_name -> Vec<(time, value)>
    let mut param_values: HashMap<String, Vec<(DateTime<Utc>, f64)>> = HashMap::new();

    for row in &rows {
        let time = row.time.with_timezone(&Utc);
        if time_set.insert(time) {
            times_ordered.push(time);
        }

        // Map DB parameter_id back to public name
        let param_uuid = row.param_id.parse::<Uuid>().ok();
        let public_name = param_uuid
            .and_then(|uuid| id_to_public.get(&uuid))
            .map(|(name, _)| name.to_string())
            .unwrap_or_else(|| row.param_id.clone());

        param_values
            .entry(public_name)
            .or_default()
            .push((time, row.value));
    }

    times_ordered.sort_unstable();

    let time_index: HashMap<DateTime<Utc>, usize> = times_ordered
        .iter()
        .enumerate()
        .map(|(i, t)| (*t, i))
        .collect();

    let num_times = times_ordered.len();

    // Build output in the order the resolved params appear (sorted by public name)
    // Deduplicate public names (multiple DB params can map to same public name)
    let mut seen_names: Vec<String> = Vec::new();
    for rp in resolved.iter() {
        if !seen_names.contains(&rp.public_name) {
            seen_names.push(rp.public_name.clone());
        }
    }

    let mut output_params: Vec<ParameterData> = Vec::new();
    for public_name in &seen_names {
        let units = resolved
            .iter()
            .find(|rp| &rp.public_name == public_name)
            .map(|rp| rp.public_units.as_str())
            .unwrap_or("");

        let mut values = vec![None; num_times];
        if let Some(readings) = param_values.get(public_name.as_str()) {
            for (time, value) in readings {
                if let Some(&idx) = time_index.get(time) {
                    values[idx] = Some(*value);
                }
            }
        }
        output_params.push(ParameterData {
            name: public_name.clone(),
            units: units.to_string(),
            values,
        });
    }

    let times_formatted: Vec<String> = times_ordered.iter().map(|t| format_time(*t)).collect();
    Ok((times_formatted, output_params))
}

// ============================================================================
// Streaming CSV/NDJSON Builders (delegated to bulk.rs)
// ============================================================================

fn build_csv_response(times: Vec<String>, parameters: &[ParameterData]) -> AppResult<Response> {
    bulk::build_csv_response_with_times(times, parameters)
}

fn build_aggregates_csv(
    times: Vec<String>,
    parameters: &[ParameterAggregateData],
) -> AppResult<Response> {
    bulk::build_aggregates_csv_response_with_times(times, parameters)
}

fn build_ndjson_response(times: Vec<String>, parameters: &[ParameterData]) -> AppResult<Response> {
    bulk::build_ndjson_response_with_times(times, parameters)
}

fn build_aggregates_ndjson(
    times: Vec<String>,
    parameters: &[ParameterAggregateData],
) -> AppResult<Response> {
    bulk::build_aggregates_ndjson_response_with_times(times, parameters)
}
