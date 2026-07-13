use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
};
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, QueryOrder, Statement};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::routes::private::{parameters, site_parameters};
use crate::error::{AppError, AppResult};
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

async fn parameter_extents(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
) -> AppResult<HashMap<Uuid, ParameterExtent>> {
    // Cadence counts ride along on the extent scan: spot = grab/lab low-frequency readings;
    // continuous counts NULL (legacy untagged) and 'derived' alongside 'continuous', since those
    // series behave like continuous data on charts.
    let stmt = Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT parameter_id, MIN(time) AS min_time, MAX(time) AS max_time, COUNT(*) AS count, \
                COUNT(*) FILTER (WHERE measurement_type = 'spot') AS spot_count, \
                COUNT(*) FILTER (WHERE measurement_type IS DISTINCT FROM 'spot') AS continuous_count \
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

struct GlobalParam {
    code: String,
    name: String,
    default_units: String,
}

/// Fetch the global catalog rows (code, name, default_units) for a set of parameter ids.
async fn global_param_map(
    db: &sea_orm::DatabaseConnection,
    param_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, GlobalParam>> {
    let rows = parameters::Entity::find()
        .filter(parameters::Column::Id.is_in(param_ids.iter().copied()))
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|p| {
            (
                p.id,
                GlobalParam {
                    code: p.code,
                    name: p.name,
                    default_units: p.default_units,
                },
            )
        })
        .collect())
}

/// Build a `ParameterResponse` from a site_parameter, enriched with the global catalog
/// (code/name/units) and the per-parameter reading extent.
fn build_parameter_response(
    p: site_parameters::Model,
    globals: &HashMap<Uuid, GlobalParam>,
    extents: &HashMap<Uuid, ParameterExtent>,
) -> ParameterResponse {
    let global = globals.get(&p.parameter_id);
    let code = global.map(|g| g.code.clone()).unwrap_or_default();
    let name = global
        .map(|g| g.name.clone())
        .unwrap_or_else(|| p.name.clone());
    let units = p.display_units.clone().or_else(|| {
        global
            .map(|g| g.default_units.clone())
            .filter(|u| !u.is_empty())
    });
    let sensor_type = if p.sensor_type.is_empty() {
        p.name.clone()
    } else {
        p.sensor_type.clone()
    };
    let extent = extents.get(&p.parameter_id);
    let has_spot = extent.is_some_and(|e| e.spot_count > 0);
    let has_continuous = extent.is_some_and(|e| e.continuous_count > 0);
    let frequency = match (has_continuous, has_spot) {
        (false, true) => "low",
        (true, true) => "mixed",
        _ => "high",
    }
    .to_string();
    ParameterResponse {
        id: p.id,
        parameter_id: p.parameter_id,
        code,
        name,
        units,
        is_derived: p.is_derived.unwrap_or(false),
        sensor_type,
        display_units: p.display_units,
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
    let globals = global_param_map(&state.db, &param_ids).await?;
    let extents = parameter_extents(&state.db, site.id).await?;

    let response: Vec<ParameterResponse> = params_list
        .into_iter()
        .map(|p| build_parameter_response(p, &globals, &extents))
        .collect();

    Ok(Json(response))
}

#[derive(Debug, FromQueryResult)]
struct DataRangeRow {
    min_time: Option<DateTime<Utc>>,
    max_time: Option<DateTime<Utc>>,
    count: i64,
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
    let globals = global_param_map(&state.db, &param_ids).await?;
    let extents = parameter_extents(&state.db, site.id).await?;

    let parameters: Vec<ParameterResponse> = params_list
        .into_iter()
        .map(|p| build_parameter_response(p, &globals, &extents))
        .collect();

    // Query data range from readings
    let stmt = Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT MIN(time) AS min_time, MAX(time) AS max_time, COUNT(*) AS count FROM readings WHERE site_id = $1",
        [site.id.into()],
    );

    let range = state
        .db
        .query_one(stmt)
        .await?
        .and_then(|row| DataRangeRow::from_query_result(&row, "").ok());

    let (data_start, data_end, reading_count) = range.map_or((None, None, 0), |r| {
        (r.min_time, r.max_time, r.count)
    });

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
