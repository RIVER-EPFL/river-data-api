use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectResponse {
    pub id: Uuid,
    pub name: String,
    #[schema(required)]
    pub description: Option<String>,
}
