use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

/// Brief project reference for embedding in responses
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ProjectRef {
    pub id: Uuid,
    pub name: String,
}

/// Brief site reference for embedding in responses
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SiteRef {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SiteResponse {
    pub id: Uuid,
    #[schema(required)]
    pub project_id: Option<Uuid>,
    #[schema(required)]
    pub subproject_id: Option<Uuid>,
    pub name: String,
    #[schema(required)]
    pub latitude: Option<f64>,
    #[schema(required)]
    pub longitude: Option<f64>,
    #[schema(required)]
    pub altitude_m: Option<f64>,
}

/// Parameter information embedded in site responses
#[derive(Debug, Serialize, ToSchema)]
pub struct ParameterResponse {
    /// Site-parameter id
    pub id: Uuid,
    /// Global catalog parameter id
    pub parameter_id: Uuid,
    /// Stable parameter code (catalog `code`, e.g. "DOmgL")
    pub code: String,
    /// Human-readable parameter name (catalog `name`, e.g. "Dissolved Oxygen")
    pub name: String,
    /// Resolved units: site override (`display_units`) falling back to the catalog `default_units`
    #[schema(required)]
    pub units: Option<String>,
    /// How this site fills the slot: 'manual' or 'tool'
    pub entry_mode: String,
    pub sensor_type: String,
    #[schema(required)]
    pub display_units: Option<String>,
    /// Display precision the client formats with; the API serves full precision.
    #[schema(required)]
    pub decimal_places: Option<i16>,
    #[schema(required)]
    pub sample_interval_sec: Option<i32>,
    #[schema(required)]
    pub is_active: Option<bool>,
    /// Earliest reading timestamp for this parameter at the site
    #[schema(required)]
    pub data_start: Option<DateTime<Utc>>,
    /// Latest reading timestamp for this parameter at the site
    #[schema(required)]
    pub data_end: Option<DateTime<Utc>>,
    /// Number of readings for this parameter at the site
    #[schema(required)]
    pub reading_count: Option<i64>,
    /// Whether any continuous (or legacy untagged) readings exist for this parameter at the site
    pub has_continuous: bool,
    /// Whether any spot (grab/lab) readings exist for this parameter at the site
    pub has_spot: bool,
    /// Data-driven cadence classification: 'low' (spot-only), 'high' (no spot), or 'mixed'.
    /// Low-frequency series render marker-only over their full range and skip the aggregate path.
    pub frequency: String,
}

/// Detailed site response with project info, parameters, and data range
#[derive(Debug, Serialize, ToSchema)]
pub struct SiteDetailResponse {
    pub id: Uuid,
    pub name: String,
    #[schema(required)]
    pub latitude: Option<f64>,
    #[schema(required)]
    pub longitude: Option<f64>,
    #[schema(required)]
    pub altitude_m: Option<f64>,
    #[schema(required)]
    pub project: Option<ProjectRef>,
    pub parameters: Vec<ParameterResponse>,
    /// Earliest reading timestamp for this site
    #[schema(required)]
    pub data_start: Option<DateTime<Utc>>,
    /// Latest reading timestamp for this site
    #[schema(required)]
    pub data_end: Option<DateTime<Utc>>,
    /// Total number of readings for this site
    pub reading_count: i64,
}
