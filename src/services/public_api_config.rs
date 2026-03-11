use moka::future::Cache;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::entity::{projects, public_exposed_parameters, sites};

/// Cache for public project configurations
pub type PublicConfigCache = Cache<String, Arc<PublicProjectConfig>>;

/// Configuration for a public project, loaded from DB
#[derive(Debug, Clone)]
pub struct PublicProjectConfig {
    pub project_id: Uuid,
    pub project_name: String,
    pub slug: String,
    pub api_title: String,
    pub api_description: String,
    pub api_version: String,
    pub contact_email: Option<String>,
    pub sites: Vec<PublicSiteConfig>,
    pub exposed_params: Vec<ExposedParamConfig>,
}

#[derive(Debug, Clone)]
pub struct PublicSiteConfig {
    pub site_id: Uuid,
    pub name: String,
    pub slug: String,
}

#[derive(Debug, Clone)]
pub struct ExposedParamConfig {
    pub public_name: String,
    pub public_units: String,
    pub parameter_id: Uuid,
    pub description: Option<String>,
    pub sort_order: i32,
    pub conversion_factor: f64,
    pub conversion_offset: f64,
    pub include_derived: bool,
}

/// Create a new public config cache with a 5-minute TTL.
pub fn new_public_config_cache() -> PublicConfigCache {
    Cache::builder()
        .max_capacity(100)
        .time_to_live(Duration::from_secs(300))
        .build()
}

/// Load or return cached public project config by slug.
pub async fn get_public_config(
    db: &DatabaseConnection,
    cache: &PublicConfigCache,
    slug: &str,
) -> Result<Arc<PublicProjectConfig>, crate::error::AppError> {
    if let Some(config) = cache.get(slug).await {
        return Ok(config);
    }

    let config = load_public_config(db, slug).await?;
    let config = Arc::new(config);
    cache.insert(slug.to_string(), config.clone()).await;
    Ok(config)
}

/// List all public project slugs (for discovery).
pub async fn list_public_slugs(
    db: &DatabaseConnection,
) -> Result<Vec<String>, crate::error::AppError> {
    let projects = projects::Entity::find()
        .filter(projects::Column::IsPublic.eq(true))
        .all(db)
        .await
        .map_err(crate::error::AppError::Database)?;

    Ok(projects.into_iter().filter_map(|p| p.public_slug).collect())
}

/// Invalidate a cached config by slug.
pub async fn invalidate_config(cache: &PublicConfigCache, slug: &str) {
    cache.invalidate(slug).await;
}

async fn load_public_config(
    db: &DatabaseConnection,
    slug: &str,
) -> Result<PublicProjectConfig, crate::error::AppError> {
    let project = projects::Entity::find()
        .filter(projects::Column::IsPublic.eq(true))
        .filter(projects::Column::PublicSlug.eq(slug))
        .one(db)
        .await
        .map_err(crate::error::AppError::Database)?
        .ok_or_else(|| {
            crate::error::AppError::NotFound(format!("Public project not found: {slug}"))
        })?;

    // Load sites with public_slug set, belonging to this project
    let db_sites = sites::Entity::find()
        .filter(sites::Column::ProjectId.eq(project.id))
        .filter(sites::Column::PublicSlug.is_not_null())
        .all(db)
        .await
        .map_err(crate::error::AppError::Database)?;

    let site_configs: Vec<PublicSiteConfig> = db_sites
        .into_iter()
        .filter_map(|s| {
            s.public_slug.map(|slug| PublicSiteConfig {
                site_id: s.id,
                name: s.name,
                slug,
            })
        })
        .collect();

    // Load exposed parameters
    let exposed = public_exposed_parameters::Entity::find()
        .filter(public_exposed_parameters::Column::ProjectId.eq(project.id))
        .order_by_asc(public_exposed_parameters::Column::SortOrder)
        .all(db)
        .await
        .map_err(crate::error::AppError::Database)?;

    let exposed_configs: Vec<ExposedParamConfig> = exposed
        .into_iter()
        .map(|e| ExposedParamConfig {
            public_name: e.public_name,
            public_units: e.public_units,
            parameter_id: e.parameter_id,
            description: e.description,
            sort_order: e.sort_order,
            conversion_factor: e.conversion_factor.unwrap_or(1.0),
            conversion_offset: e.conversion_offset.unwrap_or(0.0),
            include_derived: e.include_derived,
        })
        .collect();

    Ok(PublicProjectConfig {
        project_id: project.id,
        project_name: project.name,
        slug: slug.to_string(),
        api_title: project
            .public_api_title
            .unwrap_or_else(|| "Public API".to_string()),
        api_description: project
            .public_api_description
            .unwrap_or_else(|| "Public sensor data API.".to_string()),
        api_version: project
            .public_api_version
            .unwrap_or_else(|| "1.0.0".to_string()),
        contact_email: project.public_contact_email,
        sites: site_configs,
        exposed_params: exposed_configs,
    })
}
