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
struct ParameterExtentRow {
    parameter_id: Uuid,
    min_time: Option<DateTime<Utc>>,
    max_time: Option<DateTime<Utc>>,
    count: i64,
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

async fn parameter_extents(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
) -> AppResult<HashMap<Uuid, ParameterExtent>> {
    // Cadence counts ride along on the extent scan: spot = grab/lab low-frequency readings;
    // continuous counts NULL (legacy untagged) and 'derived' alongside 'continuous', since those
    // series behave like continuous data on charts. The continuous count mirrors the continuous
    // aggregates' population (replicate 0, unflagged) so has_continuous agrees with the rollups.
    // Spot is counted at any replicate index: grab sets are served raw, never rolled up, and a
    // replicate series need not start at 0.
    let stmt = Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT parameter_id, MIN(time) AS min_time, MAX(time) AS max_time, COUNT(*) AS count, \
                COUNT(*) FILTER (WHERE measurement_type = 'spot' \
                                   AND is_flagged IS NOT TRUE AND withdrawn_at IS NULL) AS spot_count, \
                COUNT(*) FILTER (WHERE measurement_type IS DISTINCT FROM 'spot' \
                                   AND replicate_index = 0 AND is_flagged IS NOT TRUE) AS continuous_count \
         FROM readings WHERE site_id = $1 GROUP BY parameter_id",
        [site_id.into()],
    );

    let rows = ParameterExtentRow::find_by_statement(stmt).all(db).await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.parameter_id,
                ParameterExtent {
                    data_start: r.min_time,
                    data_end: r.max_time,
                    reading_count: r.count,
                    spot_count: r.spot_count,
                    continuous_count: r.continuous_count,
                },
            )
        })
        .collect())
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
