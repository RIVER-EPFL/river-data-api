use axum::{
    Json,
    extract::{Path, State},
};
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, QueryOrder, Statement};

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::entity::site_parameters;
use crate::error::{AppError, AppResult};
use crate::routes::{resolve_site, resolve_site_with_project};

use super::types::{ParameterResponse, ProjectRef, SiteDetailResponse};

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
    if let Some(scope_project) = scope
        && site.project_id != Some(scope_project)
    {
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

    let response: Vec<ParameterResponse> = params_list
        .into_iter()
        .map(|p| {
            let sensor_type = if p.sensor_type.is_empty() { p.name.clone() } else { p.sensor_type };
            ParameterResponse {
                id: p.id,
                name: p.name,
                sensor_type,
                display_units: p.display_units,
                sample_interval_sec: p.sample_interval_sec,
                is_active: p.is_active,
            }
        })
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
    if let Some(scope_project) = scope
        && site.project_id != Some(scope_project)
    {
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

    let parameters: Vec<ParameterResponse> = params_list
        .into_iter()
        .map(|p| {
            let sensor_type = if p.sensor_type.is_empty() { p.name.clone() } else { p.sensor_type };
            ParameterResponse {
                id: p.id,
                name: p.name,
                sensor_type,
                display_units: p.display_units,
                sample_interval_sec: p.sample_interval_sec,
                is_active: p.is_active,
            }
        })
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
