use sea_orm::FromQueryResult;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

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
