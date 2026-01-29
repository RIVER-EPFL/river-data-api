use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// Brief zone reference for embedding in responses
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ZoneRef {
    pub id: Uuid,
    pub name: String,
}

/// Brief station reference for embedding in responses
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StationRef {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StationResponse {
    pub id: Uuid,
    pub zone_id: Option<Uuid>,
    pub name: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub altitude_m: Option<f64>,
}

/// Detailed station response with zone info
#[derive(Debug, Serialize, ToSchema)]
pub struct StationDetailResponse {
    pub id: Uuid,
    pub name: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub altitude_m: Option<f64>,
    pub zone: Option<ZoneRef>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct StationsQuery {
    /// Filter by zone ID
    pub zone_id: Option<Uuid>,
}
