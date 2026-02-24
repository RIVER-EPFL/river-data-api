use axum::{
    extract::{Path, Query, State},
    Json,
};
use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, QueryOrder, Statement};

use crate::common::AppState;
use crate::entity::{parameters, projects, sites};
use crate::error::AppResult;
use crate::routes::resolve_site;

use super::types::{ParameterResponse, ProjectRef, SiteDetailResponse, SiteResponse, SitesQuery};

#[derive(Debug, FromQueryResult)]
struct DataRangeRow {
    min_time: Option<DateTime<Utc>>,
    max_time: Option<DateTime<Utc>>,
    count: i64,
}

/// List all sites
#[utoipa::path(
    get,
    path = "/api/private/sites",
    params(SitesQuery),
    responses(
        (status = 200, description = "Sites retrieved successfully", body = Vec<SiteResponse>),
    ),
    tag = "sites"
)]
pub async fn list_sites(
    State(state): State<AppState>,
    Query(query): Query<SitesQuery>,
) -> AppResult<Json<Vec<SiteResponse>>> {
    let mut db_query = sites::Entity::find();

    if let Some(project_id) = query.project_id {
        db_query = db_query.filter(sites::Column::ProjectId.eq(project_id));
    }

    let sites_list = db_query
        .order_by_asc(sites::Column::Name)
        .all(&state.db)
        .await?;

    let response: Vec<SiteResponse> = sites_list
        .into_iter()
        .map(|s| SiteResponse {
            id: s.id,
            project_id: s.project_id,
            name: s.name,
            latitude: s.latitude,
            longitude: s.longitude,
            altitude_m: s.altitude_m,
        })
        .collect();

    Ok(Json(response))
}

/// Get a specific site by ID or name
#[utoipa::path(
    get,
    path = "/api/private/sites/{site_id}",
    params(
        ("site_id" = String, Path, description = "Site UUID or name"),
    ),
    responses(
        (status = 200, description = "Site retrieved successfully", body = SiteDetailResponse),
        (status = 404, description = "Site not found"),
    ),
    tag = "sites"
)]
pub async fn get_site(
    State(state): State<AppState>,
    Path(site_id): Path<String>,
) -> AppResult<Json<SiteDetailResponse>> {
    let site = resolve_site(&state.db, &site_id).await?;

    // Fetch project info if available
    let project = if let Some(project_id) = site.project_id {
        projects::Entity::find_by_id(project_id)
            .one(&state.db)
            .await?
            .map(|p| ProjectRef {
                id: p.id,
                name: p.name,
            })
    } else {
        None
    };

    // Fetch parameters for this site
    let params_list = parameters::Entity::find()
        .filter(parameters::Column::SiteId.eq(site.id))
        .filter(parameters::Column::IsActive.eq(true))
        .order_by_asc(parameters::Column::Name)
        .all(&state.db)
        .await?;

    let params: Vec<ParameterResponse> = params_list
        .into_iter()
        .map(|p| ParameterResponse {
            id: p.id,
            name: p.name,
            sensor_type: p.sensor_type,
            display_units: p.display_units,
            sample_interval_sec: p.sample_interval_sec,
            is_active: p.is_active,
        })
        .collect();

    // Get data time range and count for this site's parameters
    let sql = format!(
        "SELECT MIN(r.time) as min_time, MAX(r.time) as max_time, COUNT(*) as count
         FROM readings r
         JOIN parameters p ON r.parameter_id = p.id
         WHERE p.site_id = '{}'",
        site.id
    );

    let data_range = state
        .db
        .query_one(Statement::from_string(sea_orm::DatabaseBackend::Postgres, sql))
        .await?
        .and_then(|row| DataRangeRow::from_query_result(&row, "").ok());

    let (data_start, data_end, reading_count) = data_range
        .map_or((None, None, 0), |r| (r.min_time, r.max_time, r.count));

    Ok(Json(SiteDetailResponse {
        id: site.id,
        name: site.name,
        latitude: site.latitude,
        longitude: site.longitude,
        altitude_m: site.altitude_m,
        project,
        parameters: params,
        data_start,
        data_end,
        reading_count,
    }))
}

/// List parameters for a site
#[utoipa::path(
    get,
    path = "/api/private/sites/{site_id}/parameters",
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
) -> AppResult<Json<Vec<ParameterResponse>>> {
    let site = resolve_site(&state.db, &site_id).await?;

    let params_list = parameters::Entity::find()
        .filter(parameters::Column::SiteId.eq(site.id))
        .filter(parameters::Column::IsActive.eq(true))
        .order_by_asc(parameters::Column::Name)
        .all(&state.db)
        .await?;

    let response: Vec<ParameterResponse> = params_list
        .into_iter()
        .map(|p| ParameterResponse {
            id: p.id,
            name: p.name,
            sensor_type: p.sensor_type,
            display_units: p.display_units,
            sample_interval_sec: p.sample_interval_sec,
            is_active: p.is_active,
        })
        .collect();

    Ok(Json(response))
}
