use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Serialize, ToSchema)]
pub struct ZoneResponse {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
}
