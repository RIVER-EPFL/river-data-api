use axum::{
    Json,
    extract::{Query, State},
};
use sea_orm::{DatabaseBackend, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct SearchParams {
    /// Free-text query (minimum 2 characters). Matched case-insensitively against
    /// site name, sensor serial/name, parameter name/display_name, and project name.
    pub q: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SearchResponse {
    pub query: String,
    pub results: SearchResults,
    pub total: usize,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SearchResults {
    pub sites: Vec<SiteResult>,
    pub sensors: Vec<SensorResult>,
    pub parameters: Vec<ParameterResult>,
    pub projects: Vec<ProjectResult>,
}

#[derive(Debug, Serialize, FromQueryResult, ToSchema)]
pub struct SiteResult {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Serialize, FromQueryResult, ToSchema)]
pub struct SensorResult {
    pub id: Uuid,
    #[schema(required)]
    pub serial_number: Option<String>,
    #[schema(required)]
    pub name: Option<String>,
}

#[derive(Debug, Serialize, FromQueryResult, ToSchema)]
pub struct ParameterResult {
    pub id: Uuid,
    pub code: String,
    pub name: String,
}

#[derive(Debug, Serialize, FromQueryResult, ToSchema)]
pub struct ProjectResult {
    pub id: Uuid,
    pub name: String,
}

/// Cross-entity full-text search. Matches against sites, sensors, parameters, and
/// projects by name (case-insensitive substring). Requires `read_metadata`.
/// The `ILIKE` disjunction over an entity's declared fulltext columns, as `$1`. The columns come
/// from the entity rather than from this file, so a `#[crudcrate(fulltext)]` added to a model is
/// searched here too.
fn fulltext_ilike<R: crudcrate::CRUDResource>(table: &str) -> String {
    let columns = R::fulltext_searchable_columns();
    columns
        .iter()
        .map(|(name, _)| format!("{table}.{name} ILIKE $1"))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[utoipa::path(
    get,
    path = "/api/search",
    params(SearchParams),
    responses(
        (status = 200, description = "Matching entities grouped by type", body = SearchResponse),
        (status = 400, description = "Query shorter than 2 characters"),
    ),
    tag = "search"
)]
pub async fn search(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(params): Query<SearchParams>,
) -> AppResult<Json<SearchResponse>> {
    let query = params.q.trim();

    if query.len() < 2 {
        return Err(AppError::BadRequest(
            "Search query must be at least 2 characters".to_string(),
        ));
    }

    if query.len() > 200 {
        return Err(AppError::BadRequest(
            "Search query too long (max 200 characters)".to_string(),
        ));
    }

    let pattern = format!("%{query}%");

    // A project-scoped key sees only its own project, that project's sites, and sensors deployed
    // there. The global measurement catalog (`parameters`) is shared reference data, not project
    // data, so it stays visible to everyone. Unscoped principals (Keycloak users, unscoped tokens)
    // search across all projects unchanged.
    let site_match = fulltext_ilike::<crate::routes::private::sites::Site>("sites");
    let sensor_match = fulltext_ilike::<crate::routes::private::sensors::Sensor>("sensors");
    let project_match = fulltext_ilike::<crate::routes::private::projects::Project>("projects");
    let parameter_match =
        fulltext_ilike::<crate::routes::private::parameters::Parameter>("parameters");
    let (sites_sql, sensors_sql, projects_sql) = if scope.is_restricted() {
        (
            format!(
                "SELECT id, name FROM sites WHERE ({site_match}) AND project_id = ANY($2) \
                 ORDER BY name LIMIT 10"
            ),
            format!(
                "SELECT id, serial_number, name FROM sensors WHERE ({sensor_match}) \
                   AND EXISTS (SELECT 1 FROM sensor_deployments d JOIN sites s ON s.id = d.site_id \
                               WHERE d.sensor_id = sensors.id AND s.project_id = ANY($2)) \
                 ORDER BY serial_number LIMIT 10"
            ),
            format!(
                "SELECT id, name FROM projects WHERE ({project_match}) AND id = ANY($2) \
                 ORDER BY name LIMIT 10"
            ),
        )
    } else {
        (
            format!("SELECT id, name FROM sites WHERE ({site_match}) ORDER BY name LIMIT 10"),
            format!(
                "SELECT id, serial_number, name FROM sensors WHERE ({sensor_match}) \
                 ORDER BY serial_number LIMIT 10"
            ),
            format!("SELECT id, name FROM projects WHERE ({project_match}) ORDER BY name LIMIT 10"),
        )
    };
    let scoped_vals = || -> Vec<sea_orm::Value> {
        match scope.sql_project_array() {
            Some(projects) => vec![pattern.clone().into(), projects],
            None => vec![pattern.clone().into()],
        }
    };

    let (sites, sensors, parameters, projects) = tokio::try_join!(
        SiteResult::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            &sites_sql,
            scoped_vals(),
        ))
        .all(&state.db),
        SensorResult::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            &sensors_sql,
            scoped_vals(),
        ))
        .all(&state.db),
        ParameterResult::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            &format!("SELECT id, code, name FROM parameters WHERE ({parameter_match}) ORDER BY code LIMIT 10"),
            [pattern.clone().into()],
        ))
        .all(&state.db),
        ProjectResult::find_by_statement(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            &projects_sql,
            scoped_vals(),
        ))
        .all(&state.db),
    )?;

    let total = sites.len() + sensors.len() + parameters.len() + projects.len();

    Ok(Json(SearchResponse {
        query: query.to_string(),
        results: SearchResults {
            sites,
            sensors,
            parameters,
            projects,
        },
        total,
    }))
}
