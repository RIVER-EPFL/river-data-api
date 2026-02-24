pub mod sites;

use std::collections::HashMap;
use std::sync::LazyLock;

use axum::{routing::get, Router};
use sea_orm::{ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter, sea_query::Expr};
use utoipa::OpenApi;
use utoipa_scalar::{Scalar, Servable};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::{projects as projects_entity, sites as sites_entity};
use crate::error::{AppError, AppResult};

// ============================================================================
// Static Configuration
// ============================================================================

pub static PROJECTS: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert("mount_resilience", "Mount Resilience");
    m
});

pub static SITES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert("les_dailles", "Les Dailles");
    m.insert("verbier", "Verbier");
    m
});

pub static EXPOSED_PARAMS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    vec!["DOuM", "WaterTempdegC"]
});

pub const DOMGL_FACTOR: f64 = 0.032;

// ============================================================================
// OpenAPI Documentation
// ============================================================================

#[derive(OpenApi)]
#[openapi(
    paths(
        sites::list_sites,
        sites::get_site,
        sites::list_parameters,
        sites::get_readings,
        sites::get_aggregates,
    ),
    components(
        schemas(
            sites::SiteRef,
            sites::ParameterInfo,
            sites::SiteDetailResponse,
            sites::ReadingsResponse,
            sites::ParameterData,
            sites::AggregatesResponse,
            sites::ParameterAggregateData,
        )
    ),
    info(
        title = "RIVER Sensor Data API",
        description = "Environmental sensor time-series data.",
        version = "1.0.0"
    )
)]
pub struct MountResilienceApiDoc;

// ============================================================================
// Router
// ============================================================================

pub fn mount_resilience_router() -> Router<AppState> {
    Router::new()
        .route("/sites", get(sites::list_sites))
        .route("/sites/{site_id}", get(sites::get_site))
        .route("/sites/{site_id}/parameters", get(sites::list_parameters))
        .route("/sites/{site_id}/readings", get(sites::get_readings))
        .route("/sites/{site_id}/aggregates/{resolution}", get(sites::get_aggregates))
        .merge(Scalar::with_url("/docs", MountResilienceApiDoc::openapi()))
}

// ============================================================================
// Resolution Helpers
// ============================================================================

/// Resolve a public API project by slug or UUID.
pub async fn resolve_public_project(
    db: &DatabaseConnection,
    project_id: &str,
) -> AppResult<(String, projects_entity::Model)> {
    if let Ok(uuid) = project_id.parse::<Uuid>() {
        let project = projects_entity::Entity::find_by_id(uuid)
            .one(db)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Project not found: {project_id}")))?;

        let slug = PROJECTS
            .iter()
            .find(|&(_, &name)| name == project.name)
            .map_or("unknown", |(&s, _)| s);

        return Ok((slug.to_string(), project));
    }

    let (slug, db_name) = PROJECTS
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(project_id))
        .ok_or_else(|| AppError::NotFound(format!("Unknown project: {project_id}")))?;

    let project = projects_entity::Entity::find()
        .filter(
            Condition::all().add(
                Expr::cust_with_values("LOWER(name) = LOWER($1)", [*db_name]),
            ),
        )
        .one(db)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!("Project '{db_name}' not found in database"))
        })?;

    Ok((slug.to_string(), project))
}

/// Resolve a public API site by slug or UUID.
pub async fn resolve_public_site(
    db: &DatabaseConnection,
    site_id: &str,
) -> AppResult<(String, sites_entity::Model)> {
    let mount_resilience = projects_entity::Entity::find()
        .filter(
            Condition::all().add(
                Expr::cust_with_values("LOWER(name) = LOWER($1)", ["Mount Resilience"]),
            ),
        )
        .one(db)
        .await?
        .ok_or_else(|| {
            AppError::NotFound("Mount Resilience project not found in database".to_string())
        })?;

    if let Ok(uuid) = site_id.parse::<Uuid>() {
        let site = sites_entity::Entity::find_by_id(uuid)
            .filter(sites_entity::Column::ProjectId.eq(mount_resilience.id))
            .one(db)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Site not found: {site_id}")))?;

        let slug = SITES
            .iter()
            .find(|&(_, &name)| name == site.name)
            .map_or("unknown", |(s, _)| *s);

        return Ok((slug.to_string(), site));
    }

    let (slug, db_name) = SITES
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(site_id))
        .ok_or_else(|| AppError::NotFound(format!("Unknown site: {site_id}")))?;

    let site = sites_entity::Entity::find()
        .filter(
            Condition::all()
                .add(Expr::cust_with_values("LOWER(name) = LOWER($1)", [*db_name]))
                .add(sites_entity::Column::ProjectId.eq(mount_resilience.id)),
        )
        .one(db)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "Site '{db_name}' not found in Mount Resilience project"
            ))
        })?;

    Ok((slug.to_string(), site))
}
