use moka::future::Cache;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, FromQueryResult, QueryFilter,
};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::routes::private::{projects, sites};

/// The version of the public serving contract this code implements: what the readings and
/// aggregates arms select and how they express it. Bumped when a serving predicate or an output
/// shape changes.
///
/// 2.3.0: spot instants are served as sample statistics (the unflagged replicate mean, with the
/// lowest unflagged replicate as the no-sample fallback), so which replicate index exists or is
/// flagged no longer decides whether an instant is served or which value it carries.
///
/// 2.4.0: `include_sample_stats` publishes n, mean, sd, min and max per instant, and a slot
/// declaring `decimal_places` has every served value, statistic and aggregate expressed at those
/// places.
///
/// 3.0.0: the sd is the sample sd (n-1), published for every instant under `sd_sample`
/// (`{code}_sd_sample` in CSV and NDJSON), and the divisor field and its column are gone.
///
/// 3.1.0: a continuous instant several streams feed on one slot is one served point at the mean
/// of their readings, with `include_sample_stats` giving the pool's n, mean, sd, min and max, and
/// the site detail counts a spot instant once however many streams feed it.
///
/// A slot declaring none is served as stored, and that is a decision rather than an omission
/// (Q124): the platform default of two places belongs to the forms, and the public arm never
/// rounds a value nobody declared a precision for.
pub const SERVING_CONTRACT_VERSION: &str = "3.1.0";

/// Cache for public project configurations
pub type PublicConfigCache = Cache<String, Arc<PublicProjectConfig>>;

/// Configuration for a public project, loaded from DB
#[derive(Debug, Clone)]
pub struct PublicProjectConfig {
    pub project_id: Uuid,
    pub project_name: String,
    pub code: String,
    pub api_title: String,
    pub api_description: String,
    /// The version the docs advertise: the project's pin when set, else the serving contract.
    pub api_version: String,
    /// `projects.public_api_version`, a per-project pin over what the docs advertise. It never
    /// changes what is served, which is why the spec carries `SERVING_CONTRACT_VERSION` beside it.
    pub version_override: Option<String>,
    pub contact_email: Option<String>,
    pub sites: Vec<PublicSiteConfig>,
    pub exposed_params: Vec<ExposedParamConfig>,
}

#[derive(Debug, Clone)]
pub struct PublicSiteConfig {
    pub site_id: Uuid,
    pub name: String,
    pub code: String,
}

#[derive(Debug, Clone)]
pub struct ExposedParamConfig {
    pub parameter_id: Uuid,
    pub code: String,
    pub name: String,
    pub units: String,
    pub site_id: Uuid,
    /// The slot's declared decimal places; None serves unrounded.
    pub decimal_places: Option<i16>,
}

/// Create a new public config cache with a 5-minute TTL.
#[must_use]
pub fn new_public_config_cache() -> PublicConfigCache {
    Cache::builder()
        .max_capacity(100)
        .time_to_live(Duration::from_secs(300))
        .build()
}

/// Load or return cached public project config by code.
pub async fn get_public_config(
    db: &DatabaseConnection,
    cache: &PublicConfigCache,
    code: &str,
) -> Result<Arc<PublicProjectConfig>, crate::error::AppError> {
    if let Some(config) = cache.get(code).await {
        return Ok(config);
    }

    let config = load_public_config(db, code).await?;
    let config = Arc::new(config);
    cache.insert(code.to_string(), config.clone()).await;
    Ok(config)
}

/// List all public project codes (for discovery).
pub async fn list_public_codes(
    db: &DatabaseConnection,
) -> Result<Vec<String>, crate::error::AppError> {
    let projects = projects::Entity::find()
        .filter(projects::Column::IsPublic.eq(true))
        .all(db)
        .await
        .map_err(crate::error::AppError::Database)?;

    Ok(projects.into_iter().filter_map(|p| p.public_code).collect())
}

/// Invalidate a cached config by code.
pub async fn invalidate_config(cache: &PublicConfigCache, code: &str) {
    cache.invalidate(code).await;
}

async fn load_public_config(
    db: &DatabaseConnection,
    code: &str,
) -> Result<PublicProjectConfig, crate::error::AppError> {
    let project = projects::Entity::find()
        .filter(projects::Column::IsPublic.eq(true))
        .filter(projects::Column::PublicCode.eq(code))
        .one(db)
        .await
        .map_err(crate::error::AppError::Database)?
        .ok_or_else(|| {
            crate::error::AppError::NotFound(format!("Public project not found: {code}"))
        })?;

    let db_sites = sites::Entity::find()
        .filter(sites::Column::ProjectId.eq(project.id))
        .filter(sites::Column::PublicCode.is_not_null())
        .all(db)
        .await
        .map_err(crate::error::AppError::Database)?;

    let site_configs: Vec<PublicSiteConfig> = db_sites
        .into_iter()
        .filter_map(|s| {
            s.public_code.map(|code| PublicSiteConfig {
                site_id: s.id,
                name: s.name,
                code,
            })
        })
        .collect();

    // Load public site_parameters joined with parameters for name/units
    #[derive(Debug, FromQueryResult)]
    struct ExposedRow {
        parameter_id: Uuid,
        site_id: Uuid,
        param_code: String,
        param_name: String,
        default_units: String,
        decimal_places: Option<i16>,
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
            "SELECT sp.parameter_id, sp.site_id, p.code AS param_code, p.name AS param_name, \
                    p.default_units, sp.decimal_places \
             FROM site_parameters sp \
             JOIN parameters p ON p.id = sp.parameter_id \
             WHERE sp.is_public = true AND sp.site_id IN ({}) \
             ORDER BY p.code",
            placeholders.join(", ")
        );
        let stmt = sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        );
        let rows: Vec<ExposedRow> = db
            .query_all_raw(stmt)
            .await
            .map_err(crate::error::AppError::Database)?
            .into_iter()
            .filter_map(|row| ExposedRow::from_query_result(&row, "").ok())
            .collect();
        rows.into_iter()
            .map(|r| ExposedParamConfig {
                parameter_id: r.parameter_id,
                code: r.param_code,
                name: r.param_name,
                units: r.default_units,
                site_id: r.site_id,
                decimal_places: r.decimal_places,
            })
            .collect()
    };

    Ok(PublicProjectConfig {
        project_id: project.id,
        project_name: project.name,
        code: code.to_string(),
        api_title: project
            .public_api_title
            .unwrap_or_else(|| "Public API".to_string()),
        api_description: project
            .public_api_description
            .unwrap_or_else(|| "Public sensor data API.".to_string()),
        api_version: project
            .public_api_version
            .clone()
            .unwrap_or_else(|| SERVING_CONTRACT_VERSION.to_string()),
        version_override: project.public_api_version,
        contact_email: project.public_contact_email,
        sites: site_configs,
        exposed_params: exposed_configs,
    })
}
