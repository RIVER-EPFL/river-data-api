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
    pub project_id: Option<Uuid>,
    pub subproject_id: Option<Uuid>,
    pub name: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
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
    pub units: Option<String>,
    /// Whether this is a derived (computed) parameter at this site
    pub is_derived: bool,
    pub sensor_type: String,
    pub display_units: Option<String>,
    pub sample_interval_sec: Option<i32>,
    pub is_active: Option<bool>,
    /// Earliest reading timestamp for this parameter at the site
    pub data_start: Option<DateTime<Utc>>,
    /// Latest reading timestamp for this parameter at the site
    pub data_end: Option<DateTime<Utc>>,
    /// Number of readings for this parameter at the site
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
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub altitude_m: Option<f64>,
    pub project: Option<ProjectRef>,
    pub parameters: Vec<ParameterResponse>,
    /// Earliest reading timestamp for this site
    pub data_start: Option<DateTime<Utc>>,
    /// Latest reading timestamp for this site
    pub data_end: Option<DateTime<Utc>>,
    /// Total number of readings for this site
    pub reading_count: i64,
}
