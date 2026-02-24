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
      title = "Mount Resilience – Alpine stream oxygen monitoring data API",
      description = "This API provides programmatic access to high frequency raw oxygen data collected by the [River Ecosystems Laboratory](https://www.epfl.ch/labs/river/) at [EPFL](https://www.epfl.ch) in alpine stream networks as part of the Mount Resilience project.\n\nThe datasets exposed through this interface correspond strictly to raw field measurements as recorded by the sensors and data loggers. Data are provided as raw logger output and are not quality controlled. No environmental corrections are applied within this API. In particular, dissolved oxygen values are not corrected for water temperature or atmospheric pressure, and percent saturation is not computed.\n\nBarometric pressure must be retrieved from the [MeteoSwiss Open Data API](https://www.meteoswiss.admin.ch/services-and-publications/service/open-data.html). The required variable is:\n\nprestas0\n\nStation: MOB\n\nThe pressure time series must be temporally matched to each oxygen measurement timestamp before applying corrections.\n\n---\n\n## Oxygen Data Correction (pseudocode)\n\nThe correction below converts temperature (T, °C) and barometric pressure (BP, Pa) into a physically consistent dissolved oxygen concentration using the Garcia Gordon solubility formulation and accounting for water vapor pressure. Salinity S defaults to 0 for freshwater.\n\n```text\nInputs:\n  T[t]   water temperature time series [°C]\n  BP[t]  barometric pressure time series [Pa], retrieved from MeteoSwiss variable prestas0 at station MOB and matched to T timestamps\n  S      salinity [PSU], default 0\n\nConstants:\n  xO2 = 0.209446\n  Pref = 101325  [Pa]\n  M_O2 = 31.9988 [g/mol]\n\nStep 1, Normalize total pressure to atm:\n  Ptotal = BP[t] / Pref\n\nStep 2, Compute saturated water vapor pressure:\n  TK   = T[t] + 273.15\n  pWSat = exp(24.4543 - 67.4509 * (100 / TK) - 4.8489 * ln(TK / 100) - 5.44e-4 * S)\n\nStep 3, Compute oxygen partial pressure:\n  pO2measured  = xO2 * (Ptotal - pWSat)\n  pO2reference = xO2 * (1.0   - pWSat)\n\nStep 4, Compute oxygen solubility at 1 atm using Garcia Gordon 1992:\n  Ts = ln((298.15 - T[t]) / (273.15 + T[t]))\n\n  lnC = A0 + A1*Ts + A2*Ts^2 + A3*Ts^3 + A4*Ts^4 + A5*Ts^5\n        + S*(B0 + B1*Ts + B2*Ts^2 + B3*Ts^3)\n        + C0*S^2\n\n  CoStar = exp(lnC) * 44.6596044945426   [μmol/L]\n\n  where coefficients:\n    A0=2.00907, A1=3.22014, A2=4.05010, A3=4.94457, A4=-0.256847, A5=3.88767\n    B0=-0.00624523, B1=-0.00737614, B2=-0.010341, B3=-0.00817083\n    C0=-4.88682e-7\n\nStep 5, Correct to in situ pressure:\n  DO_umol_L = CoStar * (pO2measured / pO2reference)\n\nStep 6, Convert to mg/L:\n  DO_mg_L = DO_umol_L * (M_O2 / 1000)\n\nOutputs:\n  DO_umol_L[t], DO_mg_L[t]\n```\n\nNote:\nAt alpine elevations, using a fixed sea level pressure instead of time resolved MeteoSwiss prestas0 data will bias oxygen saturation and derived metabolic metrics. Always use pressure data matched to the exact timestamp of the oxygen measurement.\n",
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
