use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
};
use chrono::{DateTime, Utc};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, QueryOrder, Statement,
};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::error::{AppError, AppResult};
use crate::routes::private::sites::parameters as site_parameters;
use crate::routes::{resolve_site, resolve_site_with_project};

use super::types::{ParameterResponse, ProjectRef, SiteDetailResponse};

#[derive(Debug, FromQueryResult)]
struct ContinuousExtentRow {
    parameter_id: Uuid,
    min_bucket: Option<DateTime<Utc>>,
    max_bucket: Option<DateTime<Utc>>,
    count: i64,
}

#[derive(Debug, FromQueryResult)]
struct SpotExtentRow {
    parameter_id: Uuid,
    min_time: Option<DateTime<Utc>>,
    max_time: Option<DateTime<Utc>>,
    count: i64,
}

#[derive(Debug, FromQueryResult)]
struct RecentExtentRow {
    parameter_id: Uuid,
    min_time: Option<DateTime<Utc>>,
    max_time: Option<DateTime<Utc>>,
    spot_count: i64,
    continuous_count: i64,
}

struct ParameterExtent {
    data_start: Option<DateTime<Utc>>,
    data_end: Option<DateTime<Utc>>,
    reading_count: i64,
    spot_count: i64,
    continuous_count: i64,
}

fn min_opt(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (some, None) | (None, some) => some,
    }
}

fn max_opt(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (some, None) | (None, some) => some,
    }
}

/// How far back the raw-readings freshness pass looks. Wide enough to cover the hourly
/// aggregate's refresh lag (one bucket + one schedule interval) many times over, narrow enough
/// that chunk exclusion keeps the scan to a handful of chunks.
const RECENT_EXTENT_DAYS: i64 = 14;

/// Per-parameter data extents and cadence counts, assembled from the maintained summaries
/// instead of a full hypertable scan: an unbounded `readings` query pays a planning cost
/// proportional to the chunk count on every call, which is what made this endpoint the slow
/// half of a site page load.
///
/// - Continuous extents and counts come from `readings_hourly`, whose population (replicate 0,
///   unflagged, non-spot) is exactly what `continuous_count` mirrors.
/// - Spot extents and replicate counts come from `samples`, the materialised per-instant groups.
/// - A bounded raw pass over the last `RECENT_EXTENT_DAYS` covers rows the hourly aggregate has
///   not refreshed yet and decides `has_*` for brand-new slots; it carries no flag filter, so a
///   freshly flagged tail still extends the extent.
/// - `data_streams.last_data_time` supplies a flag-agnostic newest instant per slot, so a series
///   whose trailing rows are all flagged and older than the raw pass still reports its true end.
///   The extents seed the chart range slider, and flagged points are drawn and exported, so the
///   flagged tail must stay inside the range.
///
/// `data_end` from the aggregate alone is the last bucket start plus one bucket, which can
/// overstate by up to an hour; the exact sources win whenever they are newer. `reading_count`
/// counts the summarised populations (unflagged continuous plus spot replicates), not raw rows.
async fn parameter_extents(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
) -> AppResult<HashMap<Uuid, ParameterExtent>> {
    let continuous = ContinuousExtentRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT parameter_id, MIN(bucket) AS min_bucket, MAX(bucket) AS max_bucket, \
                COALESCE(SUM(count), 0)::bigint AS count \
         FROM readings_hourly WHERE site_id = $1 AND parameter_id IS NOT NULL \
         GROUP BY parameter_id",
        [site_id.into()],
    ))
    .all(db)
    .await?;

    let spot = SpotExtentRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT parameter_id, MIN(collected_at) AS min_time, MAX(collected_at) AS max_time, \
                COALESCE(SUM(n), 0)::bigint AS count \
         FROM samples WHERE site_id = $1 GROUP BY parameter_id",
        [site_id.into()],
    ))
    .all(db)
    .await?;

    let recent = RecentExtentRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        &format!(
            "SELECT parameter_id, MIN(time) AS min_time, MAX(time) AS max_time, \
                    COUNT(*) FILTER (WHERE measurement_type = 'spot' \
                                       AND is_flagged IS NOT TRUE AND withdrawn_at IS NULL) AS spot_count, \
                    COUNT(*) FILTER (WHERE measurement_type IS DISTINCT FROM 'spot' \
                                       AND replicate_index = 0 AND is_flagged IS NOT TRUE) AS continuous_count \
             FROM readings \
             WHERE site_id = $1 AND parameter_id IS NOT NULL \
               AND time > now() - INTERVAL '{RECENT_EXTENT_DAYS} days' \
             GROUP BY parameter_id"
        ),
        [site_id.into()],
    ))
    .all(db)
    .await?;

    let mut cursors: HashMap<Uuid, DateTime<Utc>> = HashMap::new();
    for row in db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sp.parameter_id, MAX(ds.last_data_time) AS max_time \
             FROM data_streams ds JOIN site_parameters sp ON ds.site_parameter_id = sp.id \
             WHERE sp.site_id = $1 AND ds.last_data_time IS NOT NULL \
             GROUP BY sp.parameter_id",
            [site_id.into()],
        ))
        .await?
    {
        let parameter_id: Uuid = row.try_get("", "parameter_id")?;
        if let Ok(t) = row.try_get::<DateTime<Utc>>("", "max_time") {
            cursors.insert(parameter_id, t);
        }
    }

    let mut extents: HashMap<Uuid, ParameterExtent> = HashMap::new();
    let mut last_buckets: HashMap<Uuid, DateTime<Utc>> = HashMap::new();
    for r in continuous {
        let e = extents.entry(r.parameter_id).or_insert_with(empty_extent);
        e.data_start = min_opt(e.data_start, r.min_bucket);
        if let Some(b) = r.max_bucket {
            last_buckets.insert(r.parameter_id, b);
        }
        e.continuous_count = r.count;
    }
    for r in spot {
        let e = extents.entry(r.parameter_id).or_insert_with(empty_extent);
        e.data_start = min_opt(e.data_start, r.min_time);
        e.data_end = max_opt(e.data_end, r.max_time);
        e.spot_count = r.count;
    }
    for r in recent {
        let e = extents.entry(r.parameter_id).or_insert_with(empty_extent);
        e.data_start = min_opt(e.data_start, r.min_time);
        e.data_end = max_opt(e.data_end, r.max_time);
        // The raw pass overlaps hours the aggregate already covers, so it only speaks for a slot
        // the summaries report empty: it decides `has_*` for data too new to be summarised, it
        // never adds to a summarised count.
        if e.continuous_count == 0 {
            e.continuous_count = r.continuous_count;
        }
        if e.spot_count == 0 {
            e.spot_count = r.spot_count;
        }
    }
    for (parameter_id, t) in cursors {
        let e = extents.entry(parameter_id).or_insert_with(empty_extent);
        e.data_end = max_opt(e.data_end, Some(t));
    }
    // An exact source (samples, the recent pass, a stream cursor) that has seen the newest
    // bucket names the true end; only a series none of them cover falls back to the bucket
    // plus its width, overstating by at most an hour rather than clipping the last hour off.
    for (parameter_id, bucket) in last_buckets {
        let e = extents.entry(parameter_id).or_insert_with(empty_extent);
        e.data_end = match e.data_end {
            Some(exact) if exact >= bucket => Some(exact),
            _ => max_opt(e.data_end, Some(bucket + chrono::Duration::hours(1))),
        };
    }
    for e in extents.values_mut() {
        e.reading_count = e.continuous_count + e.spot_count;
    }

    Ok(extents)
}

fn empty_extent() -> ParameterExtent {
    ParameterExtent {
        data_start: None,
        data_end: None,
        reading_count: 0,
        spot_count: 0,
        continuous_count: 0,
    }
}

/// Declared cadence per site_parameter, for slots with no data yet: paired stream
/// declarations first, then the open deployment's sensor data_frequency.
async fn declared_frequencies(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
) -> AppResult<HashMap<Uuid, &'static str>> {
    let mut map: HashMap<Uuid, &'static str> = HashMap::new();

    for row in db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT d.parameter_id, bool_or(sn.data_frequency = 'low') AS any_low, \
                    bool_or(sn.data_frequency = 'high') AS any_high \
             FROM sensor_deployments d JOIN sensors sn ON sn.id = d.sensor_id \
             WHERE d.site_id = $1 AND d.deployed_until IS NULL \
             GROUP BY d.parameter_id",
            [site_id.into()],
        ))
        .await?
    {
        let parameter_id: Option<Uuid> = row.try_get("", "parameter_id")?;
        let any_low: bool = row.try_get("", "any_low")?;
        let any_high: bool = row.try_get("", "any_high")?;
        if let Some(pid) = parameter_id {
            map.insert(
                pid,
                match (any_high, any_low) {
                    (false, true) => "low",
                    (true, true) => "mixed",
                    _ => "high",
                },
            );
        }
    }

    for row in db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sp.parameter_id, bool_or(ds.measurement_type = 'spot') AS any_spot, \
                    bool_or(ds.measurement_type <> 'spot') AS any_continuous \
             FROM site_parameters sp JOIN data_streams ds ON ds.site_parameter_id = sp.id \
             WHERE sp.site_id = $1 AND ds.measurement_type IS NOT NULL \
             GROUP BY sp.parameter_id",
            [site_id.into()],
        ))
        .await?
    {
        let parameter_id: Uuid = row.try_get("", "parameter_id")?;
        let any_spot: bool = row.try_get("", "any_spot")?;
        let any_continuous: bool = row.try_get("", "any_continuous")?;
        map.insert(
            parameter_id,
            match (any_continuous, any_spot) {
                (false, true) => "low",
                (true, true) => "mixed",
                _ => "high",
            },
        );
    }

    Ok(map)
}

/// Build a `ParameterResponse` from a site_parameter, enriched with the global catalog
/// (code/name/units) and the per-parameter reading extent.
fn build_parameter_response(
    p: site_parameters::Model,
    globals: &HashMap<Uuid, site_parameters::CatalogParameter>,
    extents: &HashMap<Uuid, ParameterExtent>,
    declared: &HashMap<Uuid, &'static str>,
) -> ParameterResponse {
    let d = site_parameters::SlotDescriptor::resolve(&p, globals.get(&p.parameter_id));
    let extent = extents.get(&p.parameter_id);
    let has_spot = extent.is_some_and(|e| e.spot_count > 0);
    let has_continuous = extent.is_some_and(|e| e.continuous_count > 0);
    // Observed cadence when there is data; the DECLARED tier (stream declaration, sensor
    // data_frequency) for empty slots, so a new lab parameter opens on the right chart mode.
    let frequency = match (has_continuous, has_spot) {
        (false, true) => "low",
        (true, true) => "mixed",
        (true, false) => "high",
        (false, false) => declared.get(&p.parameter_id).copied().unwrap_or("high"),
    }
    .to_string();
    ParameterResponse {
        id: p.id,
        parameter_id: p.parameter_id,
        code: d.code,
        name: d.name,
        units: d.units,
        is_derived: p.is_derived.unwrap_or(false),
        sensor_type: d.sensor_type,
        display_units: d.display_units,
        decimal_places: d.decimal_places,
        sample_interval_sec: p.sample_interval_sec,
        is_active: p.is_active,
        data_start: extent.and_then(|e| e.data_start),
        data_end: extent.and_then(|e| e.data_end),
        reading_count: extent.map(|e| e.reading_count),
        has_continuous,
        has_spot,
        frequency,
    }
}

/// List parameters for a site
#[utoipa::path(
    get,
    path = "/{site_id}/parameters",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
    ),
    responses(
        (status = 200, description = "Parameters retrieved successfully", body = Vec<ParameterResponse>),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn list_site_parameters(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<Vec<ParameterResponse>>> {
    let site = resolve_site(&state.db, &site_id).await?;

    // Enforce project scope
    if !scope.allows_project_opt(site.project_id) {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }

    let params_list = site_parameters::Entity::find()
        .filter(site_parameters::Column::SiteId.eq(site.id))
        .filter(site_parameters::Column::IsActive.eq(true))
        .order_by_asc(site_parameters::Column::Name)
        .all(&state.db)
        .await?;

    let param_ids: Vec<Uuid> = params_list.iter().map(|p| p.parameter_id).collect();
    let globals = site_parameters::catalog_map(&state.db, param_ids.iter().copied()).await?;
    let extents = parameter_extents(&state.db, site.id).await?;
    let declared = declared_frequencies(&state.db, site.id).await?;

    let response: Vec<ParameterResponse> = params_list
        .into_iter()
        .map(|p| build_parameter_response(p, &globals, &extents, &declared))
        .collect();

    Ok(Json(response))
}

/// Get detailed site information including project, parameters, and data range
#[utoipa::path(
    get,
    path = "/{site_id}/detail",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
    ),
    responses(
        (status = 200, description = "Site detail retrieved successfully", body = SiteDetailResponse),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn get_site_detail(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<SiteDetailResponse>> {
    let (site, project) = resolve_site_with_project(&state.db, &site_id).await?;

    // Enforce project scope
    if !scope.allows_project_opt(site.project_id) {
        return Err(AppError::Forbidden(
            "Token is scoped to a different project".to_string(),
        ));
    }

    // Query active parameters
    let params_list = site_parameters::Entity::find()
        .filter(site_parameters::Column::SiteId.eq(site.id))
        .filter(site_parameters::Column::IsActive.eq(true))
        .order_by_asc(site_parameters::Column::Name)
        .all(&state.db)
        .await?;

    let param_ids: Vec<Uuid> = params_list.iter().map(|p| p.parameter_id).collect();
    let globals = site_parameters::catalog_map(&state.db, param_ids.iter().copied()).await?;
    let extents = parameter_extents(&state.db, site.id).await?;
    let declared = declared_frequencies(&state.db, site.id).await?;

    let parameters: Vec<ParameterResponse> = params_list
        .into_iter()
        .map(|p| build_parameter_response(p, &globals, &extents, &declared))
        .collect();

    // The extents cover the same rows this range spans (`WHERE site_id = $1`), so folding them is
    // the ungrouped aggregate without scanning the hypertable again.
    let (data_start, data_end, reading_count) = extents.values().fold(
        (None, None, 0i64),
        |(start, end, count): (Option<DateTime<Utc>>, Option<DateTime<Utc>>, i64), e| {
            (
                min_opt(start, e.data_start),
                max_opt(end, e.data_end),
                count + e.reading_count,
            )
        },
    );

    Ok(Json(SiteDetailResponse {
        id: site.id,
        name: site.name,
        latitude: site.latitude,
        longitude: site.longitude,
        altitude_m: site.altitude_m,
        project: project.map(|p| ProjectRef {
            id: p.id,
            name: p.name,
        }),
        parameters,
        data_start,
        data_end,
        reading_count,
    }))
}
