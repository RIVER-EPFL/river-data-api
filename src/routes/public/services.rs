use moka::future::Cache;
use sea_orm::{ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, FromQueryResult, QueryFilter};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::routes::private::{projects, sites};

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
    pub parameter_id: Uuid,
    pub name: String,
    pub units: String,
    pub site_id: Uuid,
}

/// Create a new public config cache with a 5-minute TTL.
#[must_use] 
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

    // Load public site_parameters joined with parameters for name/units
    #[derive(Debug, FromQueryResult)]
    struct ExposedRow {
        parameter_id: Uuid,
        site_id: Uuid,
        param_name: String,
        default_units: String,
    }

    let site_ids: Vec<Uuid> = site_configs.iter().map(|s| s.site_id).collect();

    let exposed_configs: Vec<ExposedParamConfig> = if site_ids.is_empty() {
        Vec::new()
    } else {
        let mut placeholders = Vec::new();
        let mut values: Vec<sea_orm::Value> = Vec::new();
        for (i, id) in site_ids.iter().enumerate() {
            placeholders.push(format!("${}", i + 1));
            values.push((*id).into());
        }
        let sql = format!(
            "SELECT sp.parameter_id, sp.site_id, p.name AS param_name, p.default_units \
             FROM site_parameters sp \
             JOIN parameters p ON p.id = sp.parameter_id \
             WHERE sp.is_public = true AND sp.site_id IN ({}) \
             ORDER BY p.name",
            placeholders.join(", ")
        );
        let stmt = sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        );
        let rows: Vec<ExposedRow> = db
            .query_all(stmt)
            .await
            .map_err(crate::error::AppError::Database)?
            .into_iter()
            .filter_map(|row| ExposedRow::from_query_result(&row, "").ok())
            .collect();
        rows.into_iter()
            .map(|r| ExposedParamConfig {
                parameter_id: r.parameter_id,
                name: r.param_name,
                units: r.default_units,
                site_id: r.site_id,
            })
            .collect()
    };

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
