use axum::{
    extract::{Path, Query, State},
    http::header::{self, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, NaiveDateTime, Utc};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, Statement};
use sea_orm::sea_query::{Alias, Expr, Order, PostgresQueryBuilder, Query as SeaQuery};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};

use crate::common::AppState;
use crate::entity::{projects as projects_entity, sites as sites_entity};
use crate::error::{AppError, AppResult};

use super::{resolve_public_site, DOMGL_FACTOR, EXPOSED_PARAMS, SITES};

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
}

fn units_for_parameter(name: &str) -> &'static str {
    match name {
        "DOuM" => "\u{00b5}M",
        "DOmgL" => "mg/L",
        "WaterTempdegC" => "\u{00b0}C",
        _ => "",
    }
}

fn all_parameter_names() -> Vec<&'static str> {
    let mut names: Vec<&str> = EXPOSED_PARAMS.iter().copied().collect();
    names.push("DOmgL");
    names
}

// ============================================================================
// Shared parameter resolution
// ============================================================================

/// Parse the `parameters` query string and determine which DB params to fetch.
///
/// Returns `(requested_params, db_params, need_domgl)`.
fn resolve_requested_params(
    parameters: Option<&str>,
) -> AppResult<(Vec<String>, Vec<&'static str>, bool)> {
    let requested_params: Vec<String> = if let Some(params_str) = parameters {
        params_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        all_parameter_names().iter().map(std::string::ToString::to_string).collect()
    };

    let need_domgl = requested_params.iter().any(|p| p == "DOmgL");
    let mut db_params: Vec<&str> = EXPOSED_PARAMS
        .iter()
        .filter(|p| requested_params.contains(&p.to_string()) || (need_domgl && **p == "DOuM"))
        .copied()
        .collect();
    db_params.dedup();

    if db_params.is_empty() && !need_domgl {
        return Err(AppError::BadRequest(
            "No valid parameters requested. Available: DOuM, WaterTempdegC, DOmgL".to_string(),
        ));
    }

    Ok((requested_params, db_params, need_domgl))
}

// ============================================================================
// GET /sites — List all sites
// ============================================================================

/// List available sites.
#[utoipa::path(
    get,
    path = "/api/public/mountresilience/sites",
    responses(
        (status = 200, body = Vec<SiteRef>),
    ),
)]
pub async fn list_sites(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<SiteRef>>> {
    let project = projects_entity::Entity::find()
        .filter(Expr::cust_with_values("LOWER(name) = LOWER($1)", ["Mount Resilience"]))
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Mount Resilience project not found".into()))?;

    let db_sites = sites_entity::Entity::find()
        .filter(sites_entity::Column::ProjectId.eq(project.id))
        .all(&state.db)
        .await?;

    let sites: Vec<SiteRef> = db_sites
        .into_iter()
        .filter_map(|s| {
            SITES
                .iter()
                .find(|(_, db_name)| **db_name == s.name)
                .map(|(slug, _)| SiteRef {
                    id: slug.to_string(),
                    uuid: s.id.to_string(),
                    name: s.name,
                })
        })
        .collect();

    Ok(Json(sites))
}

// ============================================================================
// GET /sites/{site_id} — Site info
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
    path = "/api/public/mountresilience/sites/{site_id}",
    params(
        ("site_id" = String, Path, description = "Slug or UUID"),
    ),
    responses(
        (status = 200, body = SiteDetailResponse),
        (status = 404, description = "Site not found"),
    ),
)]
pub async fn get_site(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
) -> AppResult<Json<SiteDetailResponse>> {
    let (site_slug, site_model) = resolve_public_site(&state.db, &site_id).await?;

    let params: Vec<ParameterInfo> = all_parameter_names()
        .iter()
        .map(|&name| ParameterInfo {
            name: name.to_string(),
            units: units_for_parameter(name).to_string(),
        })
        .collect();

    let stmt = Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT MIN(r.time) AS min_time, MAX(r.time) AS max_time, COUNT(*) AS count \
         FROM readings r JOIN parameters p ON r.parameter_id = p.id \
         WHERE p.site_id = $1",
        vec![site_model.id.into()],
    );

    let range = state
        .db
        .query_one(stmt)
        .await?
        .and_then(|row| DataRangeRow::from_query_result(&row, "").ok());

    let (data_start, data_end, reading_count) = range
        .map_or((None, None, 0), |r| (r.min_time.map(format_time), r.max_time.map(format_time), r.count));

    Ok(Json(SiteDetailResponse {
        site: SiteRef {
            id: site_slug,
            uuid: site_model.id.to_string(),
            name: site_model.name,
        },
        parameters: params,
        data_start,
        data_end,
        reading_count,
    }))
}

// ============================================================================
// GET /{site_id}/parameters — Parameter listing
// ============================================================================

/// List available parameters for a site.
#[utoipa::path(
    get,
    path = "/api/public/mountresilience/sites/{site_id}/parameters",
    params(
        ("site_id" = String, Path, description = "Slug or UUID"),
    ),
    responses(
        (status = 200, body = Vec<ParameterInfo>),
        (status = 404, description = "Site not found"),
    ),
)]
pub async fn list_parameters(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
) -> AppResult<Json<Vec<ParameterInfo>>> {
    let _ = resolve_public_site(&state.db, &site_id).await?;

    let params: Vec<ParameterInfo> = all_parameter_names()
        .iter()
        .map(|&name| ParameterInfo {
            name: name.to_string(),
            units: units_for_parameter(name).to_string(),
        })
        .collect();

    Ok(Json(params))
}

// ============================================================================
// GET /{site_id}/readings — Raw time-series
// ============================================================================

#[derive(Debug, Deserialize, IntoParams)]
pub struct ReadingsQuery {
    /// Format: YYYY-MM-DD HH:MM:SS or ISO 8601.
    pub start: Option<String>,
    /// Format: YYYY-MM-DD HH:MM:SS or ISO 8601.
    pub end: Option<String>,
    /// Comma-separated. Available: `DOuM`, `DOmgL`, `WaterTempdegC`. Omit for all.
    pub parameters: Option<String>,
    /// json (default) or csv.
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

#[derive(Debug, Serialize, ToSchema)]
pub struct ParameterData {
    pub name: String,
    pub units: String,
    /// One value per entry in `times`. Null where no reading exists.
    pub values: Vec<Option<f64>>,
}

#[derive(Debug, FromQueryResult)]
struct ReadingRow {
    param_name: String,
    time: chrono::DateTime<chrono::FixedOffset>,
    value: f64,
}

/// Raw time-series readings.
#[utoipa::path(
    get,
    path = "/api/public/mountresilience/sites/{site_id}/readings",
    params(
        ("site_id" = String, Path, description = "Slug or UUID"),
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
    Path(site_id): Path<String>,
    Query(query): Query<ReadingsQuery>,
) -> AppResult<Response> {
    let (site_slug, site_model) = resolve_public_site(&state.db, &site_id).await?;

    let start = query.start.as_deref().map(parse_time).transpose()?;
    let end = query.end.as_deref().map(parse_time).transpose()?;

    if let (Some(s), Some(e)) = (start, end)
        && e <= s
    {
        return Err(AppError::BadRequest(
            "end time must be after start time".to_string(),
        ));
    }

    let (requested_params, db_params, need_domgl) =
        resolve_requested_params(query.parameters.as_deref())?;

    let (times_formatted, output_params) =
        fetch_readings(&state, site_model.id, &db_params, &requested_params, need_domgl, start, end).await?;

    let actual_start = times_formatted.first().cloned();
    let actual_end = times_formatted.last().cloned();

    let format = query.format.to_lowercase();
    if format.as_str() == "csv" { build_csv_response(&times_formatted, &output_params) } else {
        let response = ReadingsResponse {
            site: SiteRef {
                id: site_slug,
                uuid: site_model.id.to_string(),
                name: site_model.name,
            },
            start: actual_start,
            end: actual_end,
            times: times_formatted,
            parameters: output_params,
        };
        Ok(Json(response).into_response())
    }
}

// ============================================================================
// GET /{site_id}/aggregates/{resolution} — Aggregated data
// ============================================================================

#[derive(Debug, Deserialize, IntoParams)]
pub struct AggregatesQuery {
    /// Format: YYYY-MM-DD HH:MM:SS or ISO 8601.
    pub start: String,
    /// Format: YYYY-MM-DD HH:MM:SS or ISO 8601.
    pub end: String,
    /// Comma-separated. Available: `DOuM`, `DOmgL`, `WaterTempdegC`. Omit for all.
    pub parameters: Option<String>,
    /// json (default) or csv.
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

#[derive(Debug, Serialize, ToSchema)]
pub struct ParameterAggregateData {
    pub name: String,
    pub units: String,
    pub avg: Vec<Option<f64>>,
    pub min: Vec<Option<f64>>,
    pub max: Vec<Option<f64>>,
    pub count: Vec<i64>,
}

#[derive(Debug, FromQueryResult)]
struct AggregateRow {
    param_name: String,
    bucket: DateTime<Utc>,
    avg_value: Option<f64>,
    min_value: Option<f64>,
    max_value: Option<f64>,
    count: i64,
}

/// Aggregated time-series (hourly, daily, weekly, monthly).
#[utoipa::path(
    get,
    path = "/api/public/mountresilience/sites/{site_id}/aggregates/{resolution}",
    params(
        ("site_id" = String, Path, description = "Slug or UUID"),
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
    Path((site_id, resolution)): Path<(String, String)>,
    Query(query): Query<AggregatesQuery>,
) -> AppResult<Response> {
    let (site_slug, site_model) = resolve_public_site(&state.db, &site_id).await?;

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

    let (requested_params, db_params, need_domgl) =
        resolve_requested_params(query.parameters.as_deref())?;

    // Build parameterized query against continuous aggregate using sea_query
    let view = Alias::new(view_name);
    let a = Alias::new("a");
    let p = Alias::new("p");

    let (sql, values) = SeaQuery::select()
        .expr_as(Expr::col((p.clone(), Alias::new("name"))), Alias::new("param_name"))
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
        .and_where(Expr::col((p.clone(), Alias::new("site_id"))).eq(site_model.id))
        .and_where(Expr::col((p.clone(), Alias::new("name"))).is_in(db_params.iter().copied()))
        .and_where(Expr::col((a.clone(), Alias::new("bucket"))).gte(start))
        .and_where(Expr::col((a.clone(), Alias::new("bucket"))).lte(end))
        .order_by((a.clone(), Alias::new("bucket")), Order::Asc)
        .order_by((p.clone(), Alias::new("name")), Order::Asc)
        .build(PostgresQueryBuilder);

    let stmt = Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values.0,
    );

    let rows: Vec<AggregateRow> = state
        .db
        .query_all(stmt)
        .await?
        .into_iter()
        .filter_map(|row| AggregateRow::from_query_result(&row, "").ok())
        .collect();

    // Collect unique buckets and group by parameter
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
        param_aggs
            .entry(row.param_name.clone())
            .or_default()
            .push((row.bucket, AggValues {
                avg: row.avg_value,
                min: row.min_value,
                max: row.max_value,
                count: row.count,
            }));
    }

    times_ordered.sort_unstable();

    let time_index: HashMap<DateTime<Utc>, usize> = times_ordered
        .iter()
        .enumerate()
        .map(|(i, t)| (*t, i))
        .collect();

    let num_times = times_ordered.len();

    // Build output, including computed DOmgL aggregates
    let mut output_names: Vec<String> = Vec::new();
    if need_domgl {
        output_names.push("DOmgL".to_string());
    }
    for &exposed in EXPOSED_PARAMS.iter() {
        if requested_params.contains(&exposed.to_string()) {
            output_names.push(exposed.to_string());
        }
    }

    let mut output_params: Vec<ParameterAggregateData> = Vec::new();

    for param_name in &output_names {
        if param_name == "DOmgL" {
            // DOmgL = DOuM * DOMGL_FACTOR (linear, so applies to avg/min/max)
            let mut avg = vec![None; num_times];
            let mut min = vec![None; num_times];
            let mut max = vec![None; num_times];
            let mut count = vec![0i64; num_times];
            if let Some(doum_aggs) = param_aggs.get("DOuM") {
                for (bucket, agg) in doum_aggs {
                    if let Some(&idx) = time_index.get(bucket) {
                        avg[idx] = agg.avg.map(|v| v * DOMGL_FACTOR);
                        min[idx] = agg.min.map(|v| v * DOMGL_FACTOR);
                        max[idx] = agg.max.map(|v| v * DOMGL_FACTOR);
                        count[idx] = agg.count;
                    }
                }
            }
            output_params.push(ParameterAggregateData {
                name: "DOmgL".to_string(),
                units: units_for_parameter("DOmgL").to_string(),
                avg, min, max, count,
            });
        } else {
            let mut avg = vec![None; num_times];
            let mut min = vec![None; num_times];
            let mut max = vec![None; num_times];
            let mut count = vec![0i64; num_times];
            if let Some(aggs) = param_aggs.get(param_name.as_str()) {
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
                name: param_name.clone(),
                units: units_for_parameter(param_name).to_string(),
                avg, min, max, count,
            });
        }
    }

    let times_formatted: Vec<String> = times_ordered.iter().map(|t| format_time(*t)).collect();

    let format = query.format.to_lowercase();
    if format.as_str() == "csv" { build_aggregates_csv(&times_formatted, &output_params) } else {
        let response = AggregatesResponse {
            site: SiteRef {
                id: site_slug,
                uuid: site_model.id.to_string(),
                name: site_model.name,
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

// ============================================================================
// Shared helpers
// ============================================================================

/// Fetch raw readings, build time axis and parameter arrays (including computed `DOmgL`).
async fn fetch_readings(
    state: &AppState,
    site_id: uuid::Uuid,
    db_params: &[&str],
    requested_params: &[String],
    need_domgl: bool,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
) -> AppResult<(Vec<String>, Vec<ParameterData>)> {
    let r = Alias::new("r");
    let p = Alias::new("p");

    let mut query = SeaQuery::select();
    query
        .expr_as(Expr::col((p.clone(), Alias::new("name"))), Alias::new("param_name"))
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
        .and_where(Expr::col((p.clone(), Alias::new("site_id"))).eq(site_id))
        .and_where(Expr::col((p.clone(), Alias::new("name"))).is_in(db_params.iter().copied()))
        .order_by((r.clone(), Alias::new("time")), Order::Asc)
        .order_by((p.clone(), Alias::new("name")), Order::Asc);

    if let Some(s) = start {
        query.and_where(Expr::col((r.clone(), Alias::new("time"))).gte(s));
    }
    if let Some(e) = end {
        query.and_where(Expr::col((r.clone(), Alias::new("time"))).lte(e));
    }

    let (sql, values) = query.build(PostgresQueryBuilder);
    let stmt = Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values.0,
    );

    let rows: Vec<ReadingRow> = state
        .db
        .query_all(stmt)
        .await?
        .into_iter()
        .filter_map(|row| ReadingRow::from_query_result(&row, "").ok())
        .collect();

    let mut times_ordered: Vec<DateTime<Utc>> = Vec::new();
    let mut time_set: std::collections::HashSet<DateTime<Utc>> = std::collections::HashSet::new();
    let mut param_values: HashMap<String, Vec<(DateTime<Utc>, f64)>> = HashMap::new();

    for row in &rows {
        let time = row.time.with_timezone(&Utc);
        if time_set.insert(time) {
            times_ordered.push(time);
        }
        param_values
            .entry(row.param_name.clone())
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

    let mut output_names: Vec<String> = Vec::new();
    if need_domgl {
        output_names.push("DOmgL".to_string());
    }
    for &exposed in EXPOSED_PARAMS.iter() {
        if requested_params.contains(&exposed.to_string()) {
            output_names.push(exposed.to_string());
        }
    }

    let mut output_params: Vec<ParameterData> = Vec::new();

    for param_name in &output_names {
        if param_name == "DOmgL" {
            let mut values = vec![None; num_times];
            if let Some(doum_readings) = param_values.get("DOuM") {
                for (time, value) in doum_readings {
                    if let Some(&idx) = time_index.get(time) {
                        values[idx] = Some(*value * DOMGL_FACTOR);
                    }
                }
            }
            output_params.push(ParameterData {
                name: "DOmgL".to_string(),
                units: units_for_parameter("DOmgL").to_string(),
                values,
            });
        } else {
            let mut values = vec![None; num_times];
            if let Some(readings) = param_values.get(param_name.as_str()) {
                for (time, value) in readings {
                    if let Some(&idx) = time_index.get(time) {
                        values[idx] = Some(*value);
                    }
                }
            }
            output_params.push(ParameterData {
                name: param_name.clone(),
                units: units_for_parameter(param_name).to_string(),
                values,
            });
        }
    }

    let times_formatted: Vec<String> = times_ordered.iter().map(|t| format_time(*t)).collect();
    Ok((times_formatted, output_params))
}

// ============================================================================
// CSV Builders
// ============================================================================

fn build_csv_response(
    times: &[String],
    parameters: &[ParameterData],
) -> AppResult<Response> {
    let mut csv_data = String::new();

    csv_data.push_str("time");
    for param in parameters {
        csv_data.push(',');
        csv_data.push_str(&param.name);
    }
    csv_data.push('\n');

    for (i, time) in times.iter().enumerate() {
        csv_data.push_str(time);
        for param in parameters {
            csv_data.push(',');
            if let Some(Some(v)) = param.values.get(i) {
                csv_data.push_str(&format!("{v:.2}"));
            }
        }
        csv_data.push('\n');
    }

    Response::builder()
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/csv; charset=utf-8"),
        )
        .header(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment; filename=\"readings.csv\""),
        )
        .body(axum::body::Body::from(csv_data))
        .map_err(|e| AppError::Internal(e.to_string()))
}

fn build_aggregates_csv(
    times: &[String],
    parameters: &[ParameterAggregateData],
) -> AppResult<Response> {
    let mut csv_data = String::new();

    csv_data.push_str("time");
    for param in parameters {
        csv_data.push_str(&format!(",{}_avg,{}_min,{}_max,{}_count",
            param.name, param.name, param.name, param.name));
    }
    csv_data.push('\n');

    for (i, time) in times.iter().enumerate() {
        csv_data.push_str(time);
        for param in parameters {
            csv_data.push(',');
            if let Some(Some(v)) = param.avg.get(i) { csv_data.push_str(&format!("{v:.2}")); }
            csv_data.push(',');
            if let Some(Some(v)) = param.min.get(i) { csv_data.push_str(&format!("{v:.2}")); }
            csv_data.push(',');
            if let Some(Some(v)) = param.max.get(i) { csv_data.push_str(&format!("{v:.2}")); }
            csv_data.push(',');
            if let Some(c) = param.count.get(i) { csv_data.push_str(&c.to_string()); }
        }
        csv_data.push('\n');
    }

    Response::builder()
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/csv; charset=utf-8"),
        )
        .header(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment; filename=\"aggregates.csv\""),
        )
        .body(axum::body::Body::from(csv_data))
        .map_err(|e| AppError::Internal(e.to_string()))
}
